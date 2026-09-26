//! What PipeWire has: its sinks, which of them can be heard, which one is the
//! default, and which apps are sending sound to it.
//!
//! Read from `pw-dump`, whose JSON is the whole graph, rather than from
//! `wpctl status`, whose tree is laid out for people and changes shape between
//! releases. Nothing here runs anything: [`parse`] takes what `pw-dump`
//! printed, and everything else decides over what it found, so all of it is
//! tested against a real dump kept beside this file. Running the programs is
//! for `audio`.
//!
//! Almost everything in a dump was written by some client, and a client can
//! call itself anything: a number, which pw-dump then writes as a JSON number;
//! a thousand characters; a right-to-left override. So every property is read
//! as text whatever type it arrived as, an object that makes no sense is
//! skipped on its own rather than taking the rest of the dump with it, and a
//! name meant for a person goes through [`display_name`] first.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

/// Longest name shown on the deck, in characters.
pub const DISPLAY_LENGTH: usize = 40;
/// Most streams of one app a job acts on. Each is a process of its own, and
/// all of them share one deadline.
pub const MAX_STREAMS: usize = 16;
/// Most apps offered, and cycled through by `next_app`.
pub const MAX_APPS: usize = 32;

const NODE: &str = "PipeWire:Interface:Node";
const DEVICE: &str = "PipeWire:Interface:Device";
const METADATA: &str = "PipeWire:Interface:Metadata";
/// What an app shows as when neither its name nor its binary has anything
/// printable in it.
const UNNAMED: &str = "(unnamed)";

/// What a dump says, as far as outputs and apps go.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Graph {
    /// Every sink sound could be sent to, heard or not, most preferred first:
    /// by `priority.session`, highest first, then by name.
    pub sinks: Vec<Sink>,
    /// The `node.name` of the default sink, if there is one.
    pub default_sink: Option<String>,
    /// Every playback stream, in the dump's order.
    pub streams: Vec<Stream>,
}

/// Somewhere sound can go: speakers, headphones, an HDMI port.
#[derive(Clone, Debug, PartialEq)]
pub struct Sink {
    pub id: u32,
    /// `node.name`, which stays the same across restarts.
    pub name: String,
    /// `node.nick`, such as "Speaker"; empty when there is none.
    pub nick: String,
    /// `node.description`, such as "Tiger Lake-H HD Audio Controller
    /// Speaker"; empty when there is none.
    pub description: String,
    /// What to call it on the deck.
    pub display: String,
    /// `priority.session`: how much the session manager prefers it.
    pub priority: i64,
    /// Whether sound sent to it can be heard at all: not headphones that are
    /// not plugged in, nor an HDMI port with nothing on the end.
    pub usable: bool,
}

/// One app's playback stream.
#[derive(Clone, Debug, PartialEq)]
pub struct Stream {
    pub id: u32,
    /// `application.name`; empty when there is none.
    pub app: String,
    /// `application.process.binary`; empty when there is none.
    pub binary: String,
    pub node_name: String,
    /// Whether it is playing, rather than open and silent.
    pub running: bool,
}

/// The streams of one app, together.
#[derive(Clone, Debug, PartialEq)]
pub struct App {
    /// What the app is known by: its binary, else its name. What a knob
    /// remembers, and what finds the app again in the next dump.
    pub key: String,
    /// `application.process.binary`; empty when there is none.
    pub binary: String,
    /// Every `application.name` its streams give, for a target to match.
    pub names: Vec<String>,
    /// What to call it on the deck.
    pub display: String,
    /// Whether any of its streams is playing.
    pub running: bool,
    /// Its streams, playing ones first, at most [`MAX_STREAMS`]. Never empty.
    pub streams: Vec<u32>,
}

/// What the editor can offer as a target.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Targets {
    /// Every sink, heard or not, in the graph's order.
    pub outputs: Vec<OutputTarget>,
    /// Every app with a stream, in [`apps`]' order.
    pub apps: Vec<AppTarget>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputTarget {
    pub display: String,
    pub name: String,
    pub nick: String,
    pub description: String,
    pub usable: bool,
    pub default: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppTarget {
    /// The app's [`App::key`]: what a target naming it should say.
    pub app: String,
    pub display: String,
    pub binary: String,
    pub running: bool,
}

/// What `pw-dump` printed, as far as outputs and apps go.
///
/// Empty or unreadable output is an empty graph; so is a dump in which
/// nothing is a sink or a stream. An object with no usable id, or of a shape
/// nothing here expects, is left out without affecting the others.
pub fn parse(dump: &[u8]) -> Graph {
    if dump.iter().all(u8::is_ascii_whitespace) {
        return Graph::default();
    }
    // pw-dump passes bytes above 0x7f through as they came, so a client whose
    // name is Latin-1 would otherwise make the whole dump unreadable and take
    // every output and app with it. Borrowed, not copied, when it is UTF-8.
    let text = String::from_utf8_lossy(dump);
    let objects = match serde_json::from_str::<List<Loose<RawObject>>>(&text) {
        Ok(List(objects)) => objects,
        Err(e) => {
            log::warn!("pw-dump printed something that is not JSON: {e}");
            return Graph::default();
        }
    };
    let objects: Vec<(u32, RawObject)> = objects
        .into_iter()
        .filter_map(|Loose(object)| Some((object_id(&object.id)?, object)))
        .collect();

    let devices: HashMap<u32, Device> = objects
        .iter()
        .filter(|(_, object)| object.kind.0 == DEVICE)
        .map(|(id, object)| (*id, Device::new(&object.info.0)))
        .collect();

    let mut graph = Graph::default();
    for (id, object) in &objects {
        match object.kind.0.as_str() {
            NODE => {
                let info = &object.info.0;
                let props = &info.props.0;
                if is_sink(info) {
                    graph.sinks.extend(read_sink(*id, props, &devices));
                } else if props.media_class.0 == "Stream/Output/Audio" {
                    graph.streams.push(Stream {
                        id: *id,
                        app: props.application_name.0.clone(),
                        binary: props.application_binary.0.clone(),
                        node_name: props.node_name.0.clone(),
                        running: info.state.0 == "running",
                    });
                }
            }
            METADATA if graph.default_sink.is_none() => {
                graph.default_sink = default_sink(object);
            }
            _ => {}
        }
    }
    graph.sinks.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.name.cmp(&b.name))
    });
    graph
}

/// A name someone else chose, made fit to show: characters that print
/// nothing or reorder what follows them are dropped, runs of whitespace are
/// one space, and past `max` characters it ends in an ellipsis.
///
/// A newline would break the line on the deck's screen, a right-to-left
/// override would turn the rest of it round, and a name a thousand characters
/// long would shrink to nothing trying to fit.
pub fn display_name(text: &str, max: usize) -> String {
    let mut shown = String::new();
    let mut gap = false;
    for c in text.chars() {
        if c.is_whitespace() {
            gap = true;
        } else if !(c.is_control() || is_format(c)) {
            if gap && !shown.is_empty() {
                shown.push(' ');
            }
            gap = false;
            shown.push(c);
        }
    }
    if shown.chars().count() <= max {
        return shown;
    }
    if max == 0 {
        return String::new();
    }
    let kept: String = shown.chars().take(max - 1).collect();
    kept.trim_end().to_string() + "…"
}

/// Unicode's format characters (general category Cf): soft hyphens,
/// zero-width spaces and joiners, bidirectional marks, overrides and isolates,
/// and the like, which print nothing of their own.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{600}'..='\u{605}'
            | '\u{61c}'
            | '\u{6dd}'
            | '\u{70f}'
            | '\u{890}'..='\u{891}'
            | '\u{8e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// The outputs a knob switches between, in the order it goes through them.
///
/// With no `order`, every sink that can be heard, most preferred first. With
/// one, it is an allow-list as well as an order: each entry brings in every
/// sink that can be heard and has the entry in its nick, description or node
/// name, ignoring case, in the graph's order; a sink already brought in by an
/// earlier entry stays where it is. An entry that matches nothing is skipped:
/// headphones that are not plugged in are the everyday case of that, not a
/// mistake.
pub fn outputs<'a>(sinks: &'a [Sink], order: &[String]) -> Vec<&'a Sink> {
    let usable = sinks.iter().filter(|sink| sink.usable);
    if order.is_empty() {
        return usable.collect();
    }
    let mut chosen: Vec<&Sink> = Vec::new();
    for entry in order {
        let entry = entry.trim();
        // An empty entry would be a substring of every name.
        if entry.is_empty() {
            continue;
        }
        for sink in usable.clone() {
            if mentions(sink, entry) && !chosen.iter().any(|seen| seen.id == sink.id) {
                chosen.push(sink);
            }
        }
    }
    chosen
}

/// Which of `outputs` a turn lands on: `step` places on from the current
/// default, going round at either end. Where the default is not one of them
/// (a sink left out of the `outputs` list, or none at all) it is the first.
/// None when there are no outputs.
pub fn step_output(outputs: &[&Sink], current: Option<&str>, step: i8) -> Option<usize> {
    if outputs.is_empty() {
        return None;
    }
    let at = current.and_then(|name| outputs.iter().position(|sink| sink.name == name));
    Some(match at {
        Some(at) => {
            // Both fit in an i64 many times over.
            let count = outputs.len() as i64;
            (at as i64 + i64::from(step)).rem_euclid(count) as usize
        }
        None => 0,
    })
}

/// Which of `outputs` `set_output` names: the one whose nick is `name`,
/// ignoring case, else the first that mentions it.
///
/// Only `outputs` are candidates, so a sink that cannot be heard, or that the
/// `outputs` list leaves out, is never chosen. The reason names it all the
/// same, from `sinks`, so the message says why rather than only that nothing
/// matched.
pub fn find_output(sinks: &[Sink], outputs: &[&Sink], name: &str) -> Result<usize, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("set_output names no output".into());
    }
    let wanted = name.to_lowercase();
    let exact = |sink: &Sink| sink.nick.to_lowercase() == wanted;
    if let Some(at) = outputs.iter().position(|sink| exact(sink)) {
        return Ok(at);
    }
    if let Some(at) = outputs.iter().position(|sink| mentions(sink, name)) {
        return Ok(at);
    }
    let named = sinks
        .iter()
        .find(|sink| exact(sink))
        .or_else(|| sinks.iter().find(|sink| mentions(sink, name)));
    Err(match named {
        Some(sink) if !sink.usable => format!("{} is not connected", sink.display),
        Some(sink) => format!("{} is not one of the outputs in galdeck.toml", sink.display),
        None => format!("no output matches {name:?}"),
    })
}

/// Whether `wanted` appears in the sink's nick, description or node name,
/// ignoring case.
fn mentions(sink: &Sink, wanted: &str) -> bool {
    let wanted = wanted.to_lowercase();
    [&sink.nick, &sink.description, &sink.name]
        .iter()
        .any(|field| field.to_lowercase().contains(&wanted))
}

/// The apps sending sound, one per binary (or per name, for a stream that
/// gives no binary), in order of what they are called.
///
/// By binary rather than by name, because Electron apps tend to call
/// themselves "Chromium", and grouping by that would move Discord, Slack and
/// the browser together. At most [`MAX_APPS`], the first in that order.
pub fn apps(graph: &Graph) -> Vec<App> {
    let mut groups: Vec<(&str, Vec<&Stream>)> = Vec::new();
    for stream in &graph.streams {
        let key = if stream.binary.is_empty() {
            stream.app.as_str()
        } else {
            stream.binary.as_str()
        };
        // Nothing a target could name it by.
        if key.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, streams)) => streams.push(stream),
            None => groups.push((key, vec![stream])),
        }
    }
    let mut apps: Vec<App> = groups
        .into_iter()
        .map(|(key, streams)| app(key, streams))
        .collect();
    apps.sort_by_cached_key(|app| (app.display.to_lowercase(), app.key.to_lowercase()));
    apps.truncate(MAX_APPS);
    apps
}

fn app(key: &str, mut streams: Vec<&Stream>) -> App {
    // Playing streams first: the first stream is the one whose level and
    // mute speak for the app.
    streams.sort_by_key(|stream| (!stream.running, stream.id));
    let mut names: Vec<String> = Vec::new();
    for stream in &streams {
        if !stream.app.is_empty() && !names.contains(&stream.app) {
            names.push(stream.app.clone());
        }
    }
    let binary = streams
        .iter()
        .map(|stream| stream.binary.as_str())
        .find(|binary| !binary.is_empty())
        .unwrap_or_default()
        .to_string();
    let display = names
        .iter()
        .chain([&binary])
        .map(|name| display_name(name, DISPLAY_LENGTH))
        .find(|shown| !shown.is_empty())
        .unwrap_or_else(|| UNNAMED.into());
    App {
        key: key.to_string(),
        binary,
        names,
        display,
        running: streams.iter().any(|stream| stream.running),
        streams: streams
            .iter()
            .take(MAX_STREAMS)
            .map(|stream| stream.id)
            .collect(),
    }
}

/// The app a target names: the one whose binary is `target`, ignoring case,
/// else the first one of whose streams calls itself that. Never a partial
/// match: "fire" is not Firefox.
pub fn find_app(apps: &[App], target: &str) -> Option<usize> {
    if target.trim().is_empty() {
        return None;
    }
    let target = target.to_lowercase();
    let same = |text: &str| text.to_lowercase() == target;
    apps.iter().position(|app| same(&app.binary)).or_else(|| {
        apps.iter()
            .position(|app| app.names.iter().any(|name| same(name)))
    })
}

/// The app an untargeted knob or key acts on: the first one playing.
///
/// Never one that is only open. speech-dispatcher keeps a silent stream open
/// all the time, and turning a knob that quietly moves its volume looks like
/// a knob that does nothing.
pub fn default_app(apps: &[App]) -> Option<usize> {
    apps.iter().position(|app| app.running)
}

/// The app a job acts on: the one `wanted` names, else the default pick.
pub fn pick_app(apps: &[App], wanted: Option<&str>) -> Option<usize> {
    match wanted {
        Some(target) => find_app(apps, target),
        None => default_app(apps),
    }
}

/// The app after `current`, going round, playing or not.
///
/// A knob that has no app yet acts on the default pick, so the next one is
/// the one after that. An app that has gone since starts over from the first.
pub fn next_app(apps: &[App], current: Option<&str>) -> Option<usize> {
    if apps.is_empty() {
        return None;
    }
    let from = match current {
        Some(current) => find_app(apps, current),
        None => default_app(apps),
    };
    Some(from.map_or(0, |at| (at + 1) % apps.len()))
}

/// Everything the editor can offer as a target.
pub fn targets(graph: &Graph) -> Targets {
    Targets {
        outputs: graph
            .sinks
            .iter()
            .map(|sink| OutputTarget {
                display: sink.display.clone(),
                name: sink.name.clone(),
                nick: sink.nick.clone(),
                description: sink.description.clone(),
                usable: sink.usable,
                default: graph.default_sink.as_deref() == Some(sink.name.as_str()),
            })
            .collect(),
        apps: apps(graph)
            .into_iter()
            .map(|app| AppTarget {
                app: app.key,
                display: app.display,
                binary: app.binary,
                running: app.running,
            })
            .collect(),
    }
}

/// Whether a node is somewhere sound can be sent: a sink, or a duplex node
/// that takes input. Never `auto_null`, the stand-in PipeWire makes when
/// there is no real output at all.
fn is_sink(info: &RawInfo) -> bool {
    let props = &info.props.0;
    let class = match props.media_class.0.as_str() {
        "Audio/Sink" => true,
        "Audio/Duplex" => integer(&info.input_ports).is_some_and(|ports| ports > 0),
        _ => false,
    };
    class && props.node_name.0 != "auto_null"
}

fn read_sink(id: u32, props: &RawProps, devices: &HashMap<u32, Device>) -> Option<Sink> {
    let name = props.node_name.0.clone();
    // It could never be recognised as the default, nor named in `outputs`.
    if name.is_empty() {
        return None;
    }
    let device = object_id(&props.device_id).and_then(|id| devices.get(&id));
    Some(Sink {
        id,
        display: sink_display(props, device),
        name,
        nick: props.nick.0.clone(),
        description: props.description.0.clone(),
        priority: integer(&props.priority).unwrap_or(0),
        usable: usable(props, device),
    })
}

/// The nick, else the description without the card's name in front of it,
/// else the node name.
///
/// Every output on a card shares that name ("Tiger Lake-H HD Audio
/// Controller Speaker", "Tiger Lake-H HD Audio Controller HDMI / DisplayPort
/// 1 Output"), so it is the least telling part, and takes most of the room.
fn sink_display(props: &RawProps, device: Option<&Device>) -> String {
    let description = props.description.0.as_str();
    let short = device
        .map(|device| device.description.as_str())
        .filter(|card| !card.is_empty())
        .and_then(|card| description.strip_prefix(card))
        // Only a whole word: "USB AudioX" does not start with "USB Audio".
        .filter(|rest| rest.starts_with(|c: char| !c.is_alphanumeric()));
    [
        props.nick.0.as_str(),
        short.unwrap_or_default(),
        description,
        props.node_name.0.as_str(),
    ]
    .into_iter()
    .map(|name| display_name(name, DISPLAY_LENGTH))
    .find(|shown| !shown.is_empty())
    .unwrap_or_default()
}

/// Whether sound sent to a sink can be heard, decided the way WirePlumber 0.5
/// decides it before making a sink the default (`haveAvailableRoutes`).
///
/// A sink that is not part of a card with routes, such as a virtual one, can
/// be heard. Otherwise the card's routes in use say so outright; failing
/// those, the routes that could lead to it do, and one that may be connected
/// is enough. "unknown" counts as connected: this machine's built-in speaker
/// says exactly that, and taking it for unplugged would leave nothing.
fn usable(props: &RawProps, device: Option<&Device>) -> bool {
    let (Some(profile_device), Some(device)) = (integer(&props.card_profile_device), device) else {
        return true;
    };
    if let Some(route) = device
        .routes
        .iter()
        .find(|route| route.device == Some(profile_device))
    {
        return route.available != "no";
    }
    let mut mentioned = device
        .enum_routes
        .iter()
        .filter(|route| route.devices.contains(&profile_device))
        .peekable();
    mentioned.peek().is_none() || mentioned.any(|route| route.available != "no")
}

/// The default sink's node name, from the metadata object called "default",
/// the one WirePlumber keeps its choice in.
fn default_sink(object: &RawObject) -> Option<String> {
    if object.props.0.metadata_name.0 != "default" {
        return None;
    }
    object
        .metadata
        .0
        .iter()
        .map(|Loose(entry)| entry)
        .find(|entry| entry.subject.0 == "0" && entry.key.0 == "default.audio.sink")
        .map(|entry| entry.value.0.clone())
        .filter(|name| !name.is_empty())
}

/// An object id: from 1 up, short of `u32::MAX`, which PipeWire keeps to mean
/// no id at all.
fn object_id(text: &Text) -> Option<u32> {
    text.0
        .parse::<u32>()
        .ok()
        .filter(|id| (1..u32::MAX).contains(id))
}

fn integer(text: &Text) -> Option<i64> {
    text.0.parse().ok()
}

/// A card, as far as telling which of its sinks can be heard goes.
struct Device {
    /// `device.description`, which its sinks' descriptions start with.
    description: String,
    /// The routes in use.
    routes: Vec<Route>,
    /// Every route it has.
    enum_routes: Vec<Route>,
}

struct Route {
    /// For a route in use, the card profile device it leads to.
    device: Option<i64>,
    /// For any route, every card profile device it can lead to.
    devices: Vec<i64>,
    /// "yes", "no" or "unknown".
    available: String,
}

impl Device {
    fn new(info: &RawInfo) -> Device {
        let routes = |raw: &List<Loose<RawRoute>>| -> Vec<Route> {
            raw.0
                .iter()
                .map(|Loose(route)| Route {
                    device: integer(&route.device),
                    devices: route.devices.0.iter().filter_map(integer).collect(),
                    available: route.available.0.clone(),
                })
                .collect()
        };
        let params = &info.params.0;
        Device {
            description: info.props.0.device_description.0.clone(),
            routes: routes(&params.routes),
            enum_routes: routes(&params.enum_routes),
        }
    }
}

// What follows reads a dump without ever failing on what a value holds. Only
// JSON that is not JSON at all fails, and then the whole dump does.

/// A value read as text, whatever JSON type it arrived as: a string as it is,
/// a number or a boolean the way JSON writes it, and anything else as nothing.
#[derive(Debug, Default)]
struct Text(String);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Scalar;
        impl<'de> Visitor<'de> for Scalar {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Text, E> {
                Ok(Text(value.to_string()))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Text, E> {
                Ok(Text(value.to_string()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Text, E> {
                Ok(Text(value.to_string()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Text, E> {
                Ok(Text(value.to_string()))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Text, E> {
                Ok(Text(value.to_string()))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Text, E> {
                Ok(Text(value))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Text, E> {
                Ok(Text::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Text, A::Error> {
                IgnoredAny.visit_seq(seq).map(|_| Text::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Text, A::Error> {
                IgnoredAny.visit_map(map).map(|_| Text::default())
            }
        }
        deserializer.deserialize_any(Scalar)
    }
}

/// Visitor methods for every JSON value that is neither an array nor an
/// object, all answering `$value`.
macro_rules! scalars {
    ($value:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($value)
        }
    };
}

/// A JSON array of `T`. Anything else reads as an empty one.
#[derive(Debug, Default)]
struct List<T>(Vec<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for List<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Array<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for Array<T> {
            type Value = List<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<List<T>, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(List(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<List<T>, A::Error> {
                IgnoredAny.visit_map(map).map(|_| List(Vec::new()))
            }
            scalars!(List(Vec::new()));
        }
        deserializer.deserialize_any(Array(PhantomData))
    }
}

/// A JSON object read field by field. Anything else reads as `T::default()`.
#[derive(Debug, Default)]
struct Loose<T>(T);

/// A struct read from a JSON object by [`Loose`], one known key at a time.
trait Fields: Default {
    /// Read `key`'s value into its field, or skip it.
    fn field<'de, A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<(), A::Error>;
}

struct ObjectVisitor<T>(PhantomData<T>);

impl<'de, T: Fields> Visitor<'de> for ObjectVisitor<T> {
    type Value = Loose<T>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Loose<T>, A::Error> {
        let mut value = T::default();
        while let Some(key) = map.next_key::<String>()? {
            value.field(&key, &mut map)?;
        }
        Ok(Loose(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Loose<T>, A::Error> {
        IgnoredAny.visit_seq(seq).map(|_| Loose(T::default()))
    }
    scalars!(Loose(T::default()));
}

impl<'de, T: Fields> Deserialize<'de> for Loose<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ObjectVisitor(PhantomData))
    }
}

/// [`Fields`] for a struct whose fields are each read from one key. A key
/// given twice keeps its last value, and every other key is skipped.
macro_rules! fields {
    ($type:ty { $($key:literal => $field:ident),* $(,)? }) => {
        impl Fields for $type {
            fn field<'de, A: MapAccess<'de>>(
                &mut self,
                key: &str,
                map: &mut A,
            ) -> Result<(), A::Error> {
                match key {
                    $($key => self.$field = map.next_value()?,)*
                    _ => {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(())
            }
        }
    };
}

#[derive(Debug, Default)]
struct RawObject {
    id: Text,
    kind: Text,
    info: Loose<RawInfo>,
    /// A metadata object's own properties; a node's and a device's are in
    /// `info`.
    props: Loose<RawProps>,
    metadata: List<Loose<RawEntry>>,
}

fields!(RawObject {
    "id" => id,
    "type" => kind,
    "info" => info,
    "props" => props,
    "metadata" => metadata,
});

#[derive(Debug, Default)]
struct RawInfo {
    props: Loose<RawProps>,
    state: Text,
    input_ports: Text,
    params: Loose<RawParams>,
}

fields!(RawInfo {
    "props" => props,
    "state" => state,
    "n-input-ports" => input_ports,
    "params" => params,
});

/// The properties anything here looks at, of whichever kind of object.
#[derive(Debug, Default)]
struct RawProps {
    media_class: Text,
    node_name: Text,
    nick: Text,
    description: Text,
    priority: Text,
    device_id: Text,
    card_profile_device: Text,
    device_description: Text,
    application_name: Text,
    application_binary: Text,
    metadata_name: Text,
}

fields!(RawProps {
    "media.class" => media_class,
    "node.name" => node_name,
    "node.nick" => nick,
    "node.description" => description,
    "priority.session" => priority,
    "device.id" => device_id,
    "card.profile.device" => card_profile_device,
    "device.description" => device_description,
    "application.name" => application_name,
    "application.process.binary" => application_binary,
    "metadata.name" => metadata_name,
});

#[derive(Debug, Default)]
struct RawParams {
    routes: List<Loose<RawRoute>>,
    enum_routes: List<Loose<RawRoute>>,
}

fields!(RawParams {
    "Route" => routes,
    "EnumRoute" => enum_routes,
});

#[derive(Debug, Default)]
struct RawRoute {
    device: Text,
    devices: List<Text>,
    available: Text,
}

fields!(RawRoute {
    "device" => device,
    "devices" => devices,
    "available" => available,
});

#[derive(Debug, Default)]
struct RawEntry {
    subject: Text,
    key: Text,
    value: MetaName,
}

fields!(RawEntry {
    "subject" => subject,
    "key" => key,
    "value" => value,
});

#[derive(Debug, Default)]
struct RawName {
    name: Text,
}

fields!(RawName { "name" => name });

/// The `name` in a metadata value: `{ "name": "…" }`, or the same JSON held
/// in a string, which is how pw-dump prints a value whose type does not say
/// JSON. WirePlumber reads either.
#[derive(Debug, Default)]
struct MetaName(String);

impl<'de> Deserialize<'de> for MetaName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Value;
        impl<'de> Visitor<'de> for Value {
            type Value = MetaName;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<MetaName, A::Error> {
                let Loose(RawName { name }) = ObjectVisitor(PhantomData).visit_map(map)?;
                Ok(MetaName(name.0))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<MetaName, E> {
                let name = serde_json::from_str::<Loose<RawName>>(value)
                    .map(|Loose(RawName { name })| name.0)
                    .unwrap_or_default();
                Ok(MetaName(name))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<MetaName, A::Error> {
                IgnoredAny.visit_seq(seq).map(|_| MetaName::default())
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<MetaName, E> {
                Ok(MetaName::default())
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<MetaName, E> {
                Ok(MetaName::default())
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<MetaName, E> {
                Ok(MetaName::default())
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<MetaName, E> {
                Ok(MetaName::default())
            }
            fn visit_unit<E: de::Error>(self) -> Result<MetaName, E> {
                Ok(MetaName::default())
            }
        }
        deserializer.deserialize_any(Value)
    }
}

/// Dumps to test against: this machine's, and pieces to add to it.
#[cfg(test)]
pub(crate) mod testing {
    use serde_json::{json, Value};

    /// A real `pw-dump -N` from a laptop, trimmed to the objects that matter
    /// here, with its user and host names taken out. Five sinks on one card,
    /// of which only the speaker can be heard: the headphones and the three
    /// HDMI ports are not connected. One stream, speech-dispatcher's, open
    /// and silent. Two more cards switched off, one of which still lists a
    /// route in use, left over from before.
    pub const DUMP: &str = include_str!("testdata/pw-dump.json");

    /// The real dump's sinks, by id.
    pub const SPEAKER: u32 = 46;
    pub const HEADPHONES: u32 = 85;
    pub const HDMI_1: u32 = 54;
    pub const HDMI_2: u32 = 47;
    pub const HDMI_3: u32 = 38;
    /// The node name of the real dump's speaker, and its default.
    pub const SPEAKER_NAME: &str =
        "alsa_output.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Speaker__sink";

    /// The real dump with `extra` objects added.
    pub fn with(extra: &[Value]) -> Vec<u8> {
        let mut objects: Vec<Value> = serde_json::from_str(DUMP).unwrap();
        objects.extend(extra.iter().cloned());
        serde_json::to_vec(&objects).unwrap()
    }

    /// A dump of `objects` alone.
    pub fn dump(objects: &[Value]) -> Vec<u8> {
        serde_json::to_vec(objects).unwrap()
    }

    /// A node of `class` with these properties, taking input on `inputs`
    /// ports.
    pub fn node(id: u32, class: &str, state: &str, inputs: u32, props: Value) -> Value {
        let mut all = json!({ "media.class": class });
        merge(&mut all, props);
        json!({
            "id": id,
            "type": "PipeWire:Interface:Node",
            "info": { "n-input-ports": inputs, "state": state, "props": all },
        })
    }

    /// A sink with these properties besides its node name.
    pub fn sink(id: u32, name: &str, props: Value) -> Value {
        let mut all = json!({ "node.name": name });
        merge(&mut all, props);
        node(id, "Audio/Sink", "suspended", 2, all)
    }

    /// An app's playback stream. `app` is whatever JSON the app gave as its
    /// name, which need not be a string.
    pub fn stream(id: u32, app: Value, binary: Option<&str>, state: &str) -> Value {
        let mut props = json!({
            "application.name": app,
            "node.name": format!("stream-{id}"),
        });
        if let Some(binary) = binary {
            merge(&mut props, json!({ "application.process.binary": binary }));
        }
        node(id, "Stream/Output/Audio", state, 0, props)
    }

    /// A card with these routes in use and these routes in all.
    pub fn device(id: u32, description: &str, routes: Value, enum_routes: Value) -> Value {
        json!({
            "id": id,
            "type": "PipeWire:Interface:Device",
            "info": {
                "props": { "device.description": description, "media.class": "Audio/Device" },
                "params": { "Route": routes, "EnumRoute": enum_routes },
            },
        })
    }

    /// The metadata that says which sink is the default.
    pub fn default_metadata(id: u32, sink: &str) -> Value {
        json!({
            "id": id,
            "type": "PipeWire:Interface:Metadata",
            "props": { "metadata.name": "default" },
            "metadata": [{
                "subject": 0,
                "key": "default.audio.sink",
                "type": "Spa:String:JSON",
                "value": { "name": sink },
            }],
        })
    }

    fn merge(into: &mut Value, from: Value) {
        if let (Value::Object(into), Value::Object(from)) = (into, from) {
            into.extend(from);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::testing::*;
    use super::*;

    fn real() -> Graph {
        parse(DUMP.as_bytes())
    }

    fn ids(sinks: &[&Sink]) -> Vec<u32> {
        sinks.iter().map(|sink| sink.id).collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A card with one output route per profile device in `routes`, each
    /// `(device, available)`, and none in use.
    fn card(id: u32, routes: &[(u32, &str)]) -> Value {
        let all: Vec<Value> = routes
            .iter()
            .map(|(device, available)| {
                json!({ "direction": "Output", "devices": [device], "available": available })
            })
            .collect();
        device(id, "Card", json!([]), json!(all))
    }

    #[test]
    fn this_machine_has_five_sinks_and_only_the_speaker_can_be_heard() {
        let graph = real();
        let found: Vec<(u32, &str, bool)> = graph
            .sinks
            .iter()
            .map(|sink| (sink.id, sink.display.as_str(), sink.usable))
            .collect();
        // Most preferred first: the headphones outrank the speaker, then the
        // HDMI ports in their own order.
        assert_eq!(
            found,
            [
                (HEADPHONES, "Headphones", false),
                (SPEAKER, "Speaker", true),
                (HDMI_1, "HDMI 1", false),
                (HDMI_2, "HDMI 2", false),
                (HDMI_3, "HDMI 3", false),
            ]
        );
        assert_eq!(graph.default_sink.as_deref(), Some(SPEAKER_NAME));
        let speaker = &graph.sinks[1];
        assert_eq!(speaker.name, SPEAKER_NAME);
        assert_eq!(speaker.nick, "Speaker");
        assert_eq!(
            speaker.description,
            "Tiger Lake-H HD Audio Controller Speaker"
        );
        assert_eq!(speaker.priority, 712);
    }

    #[test]
    fn this_machine_has_one_silent_stream_and_so_no_app_to_pick() {
        let graph = real();
        assert_eq!(
            graph.streams,
            [Stream {
                id: 103,
                app: "speech-dispatcher-dummy".into(),
                binary: "sd_dummy".into(),
                node_name: "speech-dispatcher-dummy".into(),
                running: false,
            }]
        );
        let apps = apps(&graph);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].key, "sd_dummy");
        assert_eq!(apps[0].display, "speech-dispatcher-dummy");
        assert!(!apps[0].running);
        assert_eq!(apps[0].streams, [103]);
        assert_eq!(default_app(&apps), None);
        assert_eq!(pick_app(&apps, None), None);
        // Named, it is there to be turned all the same.
        assert_eq!(pick_app(&apps, Some("sd_dummy")), Some(0));
    }

    #[test]
    fn sources_and_the_null_sink_are_not_outputs_and_duplex_nodes_are_if_they_take_input() {
        let graph = parse(&dump(&[
            node(10, "Audio/Source", "idle", 0, json!({ "node.name": "mic" })),
            node(
                11,
                "Audio/Duplex",
                "idle",
                0,
                json!({ "node.name": "no-input" }),
            ),
            node(
                12,
                "Audio/Duplex",
                "idle",
                2,
                json!({ "node.name": "duplex" }),
            ),
            sink(
                13,
                "auto_null",
                json!({ "node.description": "Dummy Output" }),
            ),
            sink(14, "", json!({ "node.nick": "Nameless" })),
            node(
                15,
                "Stream/Input/Audio",
                "running",
                2,
                json!({ "node.name": "rec" }),
            ),
        ]));
        let names: Vec<&str> = graph.sinks.iter().map(|sink| sink.name.as_str()).collect();
        assert_eq!(names, ["duplex"]);
        assert!(graph.streams.is_empty());
    }

    #[test]
    fn sinks_are_ordered_by_priority_then_by_name() {
        let graph = parse(&dump(&[
            sink(1, "b", json!({ "priority.session": 100 })),
            sink(2, "a", json!({ "priority.session": 100 })),
            sink(3, "c", json!({ "priority.session": 900 })),
            sink(4, "d", json!({})),
            sink(5, "e", json!({ "priority.session": -5 })),
        ]));
        let order: Vec<u32> = graph.sinks.iter().map(|sink| sink.id).collect();
        assert_eq!(order, [3, 2, 1, 4, 5]);
    }

    #[test]
    fn a_sink_is_called_by_its_nick_else_by_its_description_without_the_card() {
        let graph = parse(&dump(&[
            device(80, "Tiger Lake-H HD Audio Controller", json!([]), json!([])),
            sink(
                1,
                "a",
                json!({ "node.nick": "Speaker", "node.description": "whatever" }),
            ),
            sink(
                2,
                "b",
                json!({
                    "node.description": "Tiger Lake-H HD Audio Controller HDMI / DisplayPort 1 Output",
                    "device.id": 80,
                }),
            ),
            // The card's name, but not as a whole word.
            sink(
                3,
                "c",
                json!({
                    "node.description": "Tiger Lake-H HD Audio Controllers",
                    "device.id": 80,
                }),
            ),
            // No card to take a name off.
            sink(
                4,
                "d",
                json!({ "node.description": "Loopback Analog Stereo" }),
            ),
            // Nothing to show but the node name.
            sink(5, "e.sink", json!({ "node.nick": "\u{200b}" })),
        ]));
        let shown = |id| {
            graph
                .sinks
                .iter()
                .find(|sink| sink.id == id)
                .map(|sink| sink.display.clone())
        };
        assert_eq!(shown(1).as_deref(), Some("Speaker"));
        assert_eq!(shown(2).as_deref(), Some("HDMI / DisplayPort 1 Output"));
        assert_eq!(
            shown(3).as_deref(),
            Some("Tiger Lake-H HD Audio Controllers")
        );
        assert_eq!(shown(4).as_deref(), Some("Loopback Analog Stereo"));
        assert_eq!(shown(5).as_deref(), Some("e.sink"));
    }

    #[test]
    fn display_names_lose_what_does_not_print_and_stay_short() {
        assert_eq!(display_name("Speaker", 40), "Speaker");
        assert_eq!(display_name("  Line\n\tOut  ", 40), "Line Out");
        assert_eq!(display_name("a\u{7}b\u{1b}[31mc", 40), "ab[31mc");
        // Bidirectional overrides and isolates, zero-width characters, and a
        // byte order mark.
        assert_eq!(
            display_name("\u{202e}evil\u{202c} \u{2066}x\u{2069}\u{200b}\u{feff}", 40),
            "evil x"
        );
        assert_eq!(display_name("soft\u{ad}hyphen", 40), "softhyphen");
        assert_eq!(display_name("\u{200b}\u{200f}", 40), "");
        // What is kept, including letters that are not ASCII, is kept whole.
        assert_eq!(display_name("Écouteurs 🎧", 40), "Écouteurs 🎧");

        let long = "x".repeat(100);
        let shown = display_name(&long, 40);
        assert_eq!(shown.chars().count(), 40);
        assert!(shown.ends_with('…'));
        assert_eq!(display_name(&"x".repeat(40), 40), "x".repeat(40));
        // No space is left hanging before the ellipsis.
        assert_eq!(display_name("abc defgh", 5), "abc…");
        assert_eq!(display_name("abc", 0), "");
        assert_eq!(display_name("abc", 1), "…");
    }

    #[test]
    fn control_characters_in_names_never_reach_the_display() {
        let graph = parse(&with(&[
            sink(
                200,
                "virtual",
                json!({ "node.nick": "Evil\u{202e}\nSink\u{0}", "priority.session": 5000 }),
            ),
            stream(201, json!("Play\u{7}er\u{200d}\r\n2"), None, "running"),
        ]));
        assert_eq!(graph.sinks[0].display, "Evil Sink");
        // The name itself is kept as given: it is what a target must match.
        assert_eq!(graph.sinks[0].nick, "Evil\u{202e}\nSink\u{0}");
        let apps = apps(&graph);
        let player = &apps[default_app(&apps).unwrap()];
        assert_eq!(player.display, "Player 2");
    }

    #[test]
    fn a_name_that_is_not_utf_8_costs_only_itself() {
        // A Latin-1 program name reaches the dump as raw bytes, in a value and
        // in a key alike; the rest of the dump must still be read.
        let mut odd = stream(204, json!("cafMARK"), Some("cafMARK"), "running");
        odd["info"]["props"]["xMARK.key"] = json!("value");
        let dump = with(&[odd]);
        let mut raw = Vec::new();
        let mut rest = dump.as_slice();
        while let Some(at) = rest.windows(4).position(|w| w == b"MARK") {
            raw.extend_from_slice(&rest[..at]);
            raw.push(0xe9);
            rest = &rest[at + 4..];
        }
        raw.extend_from_slice(rest);
        assert!(
            std::str::from_utf8(&raw).is_err(),
            "the dump must be invalid UTF-8"
        );

        let graph = parse(&raw);
        assert_eq!(graph.sinks.len(), 5);
        assert_eq!(graph.default_sink.as_deref(), Some(SPEAKER_NAME));
        let apps = apps(&graph);
        assert!(find_app(&apps, "caf\u{fffd}").is_some(), "{apps:?}");
    }

    #[test]
    fn a_numeric_application_name_is_still_an_app() {
        // pw-dump writes any value that looks like a number as one, and a
        // client can call itself anything.
        let graph = parse(&with(&[
            stream(201, json!(2048), None, "running"),
            stream(202, json!(true), Some("flag"), "idle"),
            sink(
                203,
                "numbered",
                json!({ "node.nick": 1, "priority.session": "15" }),
            ),
        ]));
        let apps = apps(&graph);
        let numbered = find_app(&apps, "2048").expect("found by its name");
        assert_eq!(apps[numbered].key, "2048");
        assert_eq!(apps[numbered].display, "2048");
        assert_eq!(apps[numbered].streams, [201]);
        let flag = find_app(&apps, "flag").unwrap();
        assert_eq!(apps[flag].names, ["true"]);
        let sink = graph.sinks.iter().find(|sink| sink.id == 203).unwrap();
        assert_eq!(sink.display, "1");
        assert_eq!(sink.priority, 15);
        // Everything that was there before is still there.
        assert_eq!(graph.sinks.len(), 6);
    }

    #[test]
    fn objects_that_make_no_sense_are_skipped_on_their_own() {
        let graph = parse(&with(&[
            json!("a string"),
            json!(null),
            json!([1, 2, 3]),
            json!({ "type": "PipeWire:Interface:Node", "info": {} }),
            sink(0, "id-zero", json!({})),
            json!({ "id": 4294967295u32, "type": "PipeWire:Interface:Node",
                    "info": { "props": { "media.class": "Audio/Sink", "node.name": "no-id" } } }),
            json!({ "id": "300", "type": "PipeWire:Interface:Node",
                    "info": { "props": { "media.class": "Audio/Sink", "node.name": "string-id" } } }),
            json!({ "id": 301, "type": "PipeWire:Interface:Node", "info": "not an object" }),
            json!({ "id": 302, "type": "PipeWire:Interface:Node",
                    "info": { "props": ["not", "an", "object"], "params": 7 } }),
            json!({ "id": 303, "type": ["PipeWire:Interface:Node"], "info": {} }),
            json!({ "id": 304, "type": "PipeWire:Interface:Device",
                    "info": { "params": { "Route": "none", "EnumRoute": [null, 1, {"devices": "x"}] } } }),
            json!({ "id": 305, "type": "PipeWire:Interface:Node",
                    "info": { "props": { "media.class": "Audio/Sink", "node.name": { "a": 1 } } } }),
        ]));
        let ids: Vec<u32> = graph.sinks.iter().map(|sink| sink.id).collect();
        assert_eq!(ids, [HEADPHONES, SPEAKER, HDMI_1, HDMI_2, HDMI_3, 300]);
        assert_eq!(graph.streams.len(), 1);
        assert_eq!(graph.default_sink.as_deref(), Some(SPEAKER_NAME));
    }

    #[test]
    fn anything_that_is_not_a_dump_is_an_empty_graph() {
        for text in [
            "",
            "  \n",
            "[]",
            "{}",
            "null",
            "42",
            "\"text\"",
            "[1, \"x\", null]",
            "not json",
            "[{\"id\": 1,",
            "[] trailing",
        ] {
            assert_eq!(parse(text.as_bytes()), Graph::default(), "{text:?}");
        }
    }

    #[test]
    fn the_default_sink_comes_only_from_the_default_metadata() {
        let entry = |subject: Value, key: &str, value: Value| json!({ "subject": subject, "key": key, "type": "Spa:String:JSON", "value": value });
        let metadata = |id: u32, name: &str, entries: Vec<Value>| {
            json!({ "id": id, "type": "PipeWire:Interface:Metadata",
                    "props": { "metadata.name": name }, "metadata": entries })
        };
        let default = |objects: &[Value]| parse(&dump(objects)).default_sink;

        assert_eq!(default(&[default_metadata(1, "a")]).as_deref(), Some("a"));
        // Another metadata object's entry of the same name is not it.
        assert_eq!(
            default(&[metadata(
                1,
                "settings",
                vec![entry(
                    json!(0),
                    "default.audio.sink",
                    json!({ "name": "a" })
                )]
            )]),
            None
        );
        // Nor is an entry about some other object, or the configured default
        // rather than the one in effect.
        assert_eq!(
            default(&[metadata(
                1,
                "default",
                vec![
                    entry(json!(5), "default.audio.sink", json!({ "name": "a" })),
                    entry(
                        json!(0),
                        "default.configured.audio.sink",
                        json!({ "name": "b" })
                    ),
                ]
            )]),
            None
        );
        // JSON held in a string is read the way WirePlumber reads it.
        assert_eq!(
            default(&[metadata(
                1,
                "default",
                vec![entry(
                    json!(0),
                    "default.audio.sink",
                    json!("{ \"name\": \"c\" }")
                ),]
            )])
            .as_deref(),
            Some("c")
        );
        assert_eq!(
            default(&[metadata(
                1,
                "default",
                vec![entry(json!(0), "default.audio.sink", json!([1])),]
            )]),
            None
        );
        // The first default metadata that says wins.
        assert_eq!(
            default(&[
                metadata(1, "default", vec![]),
                default_metadata(2, "d"),
                default_metadata(3, "e"),
            ])
            .as_deref(),
            Some("d")
        );
    }

    #[test]
    fn a_virtual_sink_with_no_card_can_be_heard() {
        let graph = parse(&with(&[sink(
            200,
            "virtual_headphones",
            json!({ "node.description": "Headphones", "priority.session": 100 }),
        )]));
        let virtual_sink = graph.sinks.iter().find(|sink| sink.id == 200).unwrap();
        assert!(virtual_sink.usable);
        assert_eq!(virtual_sink.display, "Headphones");
        // The `outputs` entry finds the virtual sink, not the headphones on
        // the card, which are not plugged in.
        assert_eq!(
            ids(&outputs(&graph.sinks, &strings(&["Headphones"]))),
            [200]
        );
        let usable = outputs(&graph.sinks, &[]);
        assert_eq!(ids(&usable), [SPEAKER, 200]);
        // The card's headphones have "Headphones" as their very nick, but
        // cannot be heard, so the virtual sink is the one set.
        assert_eq!(find_output(&graph.sinks, &usable, "Headphones"), Ok(1));
    }

    #[test]
    fn a_stale_route_on_a_card_that_is_switched_off_decides_nothing_for_another_card() {
        // The real dump's NVIDIA card is off, yet still lists an HDMI route
        // in use, one that says no. Here it names profile device 0, which on
        // the laptop card is the speaker. Were routes looked up on any card
        // rather than the sink's own, the speaker would go unheard.
        let mut objects: Vec<Value> = serde_json::from_str(DUMP).unwrap();
        for object in &mut objects {
            if object["id"] == 55 {
                object["info"]["params"]["Route"] = json!([{
                    "direction": "Output", "device": 0, "devices": [0],
                    "available": "no", "profile": 1,
                }]);
            }
        }
        let graph = parse(&dump(&objects));
        let speaker = graph.sinks.iter().find(|sink| sink.id == SPEAKER).unwrap();
        assert!(speaker.usable);
    }

    #[test]
    fn a_route_in_use_decides_before_the_routes_that_could_be() {
        let usable = |routes: Value, enum_routes: Value| {
            let graph = parse(&dump(&[
                device(9, "Card", routes, enum_routes),
                sink(
                    1,
                    "out",
                    json!({ "device.id": 9, "card.profile.device": 3 }),
                ),
            ]));
            graph.sinks[0].usable
        };
        let route = |device: u32, available: &str| json!({ "direction": "Output", "device": device, "devices": [device], "available": available });
        let possible = |devices: Value, available: &str| json!({ "direction": "Output", "devices": devices, "available": available });
        // In use and connected, whatever the list says.
        assert!(usable(
            json!([route(3, "unknown")]),
            json!([possible(json!([3]), "no")])
        ));
        // In use and not connected, whatever the list says.
        assert!(!usable(
            json!([route(3, "no")]),
            json!([possible(json!([3]), "yes")])
        ));
        // A route in use for another device says nothing about this one.
        assert!(!usable(
            json!([route(2, "yes")]),
            json!([possible(json!([3]), "no")])
        ));
        // Of the possible routes, one that may be connected is enough.
        assert!(usable(
            json!([]),
            json!([
                possible(json!([3]), "no"),
                possible(json!([1, 3]), "unknown")
            ])
        ));
        assert!(!usable(
            json!([]),
            json!([possible(json!([3]), "no"), possible(json!([1, 3]), "no")])
        ));
        // No route mentions it at all, as on a pro audio profile.
        assert!(usable(json!([]), json!([possible(json!([1]), "no")])));
        assert!(usable(json!([]), json!([])));

        // Without a card, or a card PipeWire does not know, or a profile
        // device, there is nothing to ask.
        let alone = |props: Value| {
            let graph = parse(&dump(&[card(9, &[(3, "no")]), sink(1, "out", props)]));
            graph.sinks[0].usable
        };
        assert!(!alone(json!({ "device.id": 9, "card.profile.device": 3 })));
        assert!(!alone(
            json!({ "device.id": "9", "card.profile.device": "3" })
        ));
        assert!(alone(json!({ "card.profile.device": 3 })));
        assert!(alone(json!({ "device.id": 9 })));
        assert!(alone(json!({ "device.id": 10, "card.profile.device": 3 })));
        assert!(alone(
            json!({ "device.id": 9, "card.profile.device": "three" })
        ));
    }

    #[test]
    fn the_outputs_list_allows_and_orders_only_sinks_that_can_be_heard() {
        let graph = parse(&dump(&[
            card(9, &[(0, "yes"), (1, "no"), (2, "unknown"), (3, "yes")]),
            sink(
                1,
                "alsa.speaker",
                json!({ "node.nick": "Speaker", "device.id": 9, "card.profile.device": 0, "priority.session": 900 }),
            ),
            sink(
                2,
                "alsa.headphones",
                json!({ "node.nick": "Headphones", "device.id": 9, "card.profile.device": 1, "priority.session": 800 }),
            ),
            sink(
                3,
                "alsa.hdmi1",
                json!({ "node.nick": "HDMI 1", "device.id": 9, "card.profile.device": 2, "priority.session": 700 }),
            ),
            sink(
                4,
                "alsa.hdmi2",
                json!({ "node.nick": "HDMI 2", "device.id": 9, "card.profile.device": 3, "priority.session": 600 }),
            ),
            sink(
                5,
                "bluez_output.AA",
                json!({ "node.description": "WH-1000XM4", "priority.session": 500 }),
            ),
        ]));
        let list = |order: &[&str]| ids(&outputs(&graph.sinks, &strings(order)));

        assert_eq!(list(&[]), [1, 3, 4, 5]);
        // In the list's order; within an entry, in the graph's.
        assert_eq!(list(&["hdmi", "speaker"]), [3, 4, 1]);
        // Case, and names from any of nick, description and node name.
        assert_eq!(list(&["wh-1000", "ALSA.SPEAKER"]), [5, 1]);
        // Each sink once, where it first came in.
        assert_eq!(list(&["HDMI 2", "hdmi", "HDMI 2"]), [4, 3]);
        // Unplugged and unknown entries are skipped without a word, and so
        // are blank ones, which would otherwise match everything.
        assert_eq!(list(&["Headphones", "Nothing", "", "  ", "Speaker"]), [1]);
        // An allow-list that allows nothing that can be heard is empty, not
        // everything.
        assert_eq!(list(&["Headphones"]), Vec::<u32>::new());
    }

    #[test]
    fn stepping_through_outputs_goes_round_and_starts_from_the_first_when_lost() {
        let graph = parse(&dump(&[
            sink(1, "a", json!({ "priority.session": 3 })),
            sink(2, "b", json!({ "priority.session": 2 })),
            sink(3, "c", json!({ "priority.session": 1 })),
        ]));
        let all = outputs(&graph.sinks, &[]);
        assert_eq!(step_output(&all, Some("a"), 1), Some(1));
        assert_eq!(step_output(&all, Some("c"), 1), Some(0));
        assert_eq!(step_output(&all, Some("a"), -1), Some(2));
        assert_eq!(step_output(&all, Some("b"), -1), Some(0));
        assert_eq!(step_output(&all, Some("b"), 0), Some(1));
        assert_eq!(step_output(&all, Some("b"), i8::MIN), Some(2));
        assert_eq!(step_output(&all, Some("elsewhere"), -1), Some(0));
        assert_eq!(step_output(&all, None, 1), Some(0));
        assert_eq!(step_output(&all[..1], Some("a"), 1), Some(0));
        assert_eq!(step_output(&[], Some("a"), 1), None);

        // This machine has one output, so a turn stays on it.
        let graph = real();
        let usable = outputs(&graph.sinks, &[]);
        assert_eq!(ids(&usable), [SPEAKER]);
        assert_eq!(
            step_output(&usable, graph.default_sink.as_deref(), 1),
            Some(0)
        );
    }

    #[test]
    fn set_output_prefers_an_exact_nick_then_the_first_that_mentions_it() {
        let graph = parse(&with(&[
            sink(
                200,
                "usb.headphones-pro",
                json!({ "node.nick": "Headphones Pro", "priority.session": 2000 }),
            ),
            sink(
                201,
                "usb.headphones",
                json!({ "node.nick": "headphones 2", "priority.session": 1500 }),
            ),
            sink(
                202,
                "virtual.speaker",
                json!({ "node.nick": "speaker", "priority.session": 1 }),
            ),
        ]));
        let usable = outputs(&graph.sinks, &[]);
        assert_eq!(ids(&usable), [200, 201, SPEAKER, 202]);
        let chosen = |name: &str| find_output(&graph.sinks, &usable, name).map(|at| usable[at].id);

        // An exact nick wins over an earlier sink that only contains it.
        assert_eq!(chosen("Headphones 2"), Ok(201));
        assert_eq!(chosen("SPEAKER"), Ok(SPEAKER));
        assert_eq!(chosen("headphones"), Ok(200));
        assert_eq!(chosen("virtual"), Ok(202));
        // Named, but not connected, or not there at all.
        assert_eq!(chosen("HDMI 3"), Err("HDMI 3 is not connected".into()));
        assert_eq!(
            chosen("Bluetooth"),
            Err("no output matches \"Bluetooth\"".into())
        );
        assert_eq!(chosen("  "), Err("set_output names no output".into()));

        // Left out of the `outputs` list.
        let listed = outputs(&graph.sinks, &strings(&["Speaker"]));
        assert_eq!(
            find_output(&graph.sinks, &listed, "Headphones Pro"),
            Err("Headphones Pro is not one of the outputs in galdeck.toml".into())
        );
    }

    #[test]
    fn an_app_with_three_streams_is_one_app_with_its_playing_stream_first() {
        let graph = parse(&with(&[
            stream(300, json!("Firefox"), Some("firefox"), "idle"),
            stream(210, json!("Firefox"), Some("firefox"), "running"),
            stream(205, json!("Firefox"), Some("firefox"), "idle"),
        ]));
        let apps = apps(&graph);
        assert_eq!(apps.len(), 2);
        let firefox = &apps[0];
        assert_eq!(firefox.key, "firefox");
        assert_eq!(firefox.display, "Firefox");
        assert_eq!(firefox.names, ["Firefox"]);
        assert!(firefox.running);
        assert_eq!(firefox.streams, [210, 205, 300]);
    }

    #[test]
    fn apps_are_one_per_binary_and_named_by_their_name() {
        let graph = parse(&dump(&[
            stream(1, json!("Chromium"), Some("Discord"), "running"),
            stream(2, json!("Chromium"), Some("slack"), "idle"),
            stream(3, json!("Chromium"), Some("chrome"), "idle"),
            stream(4, json!("mpv"), None, "idle"),
            stream(5, json!("mpv"), None, "idle"),
            stream(6, json!(""), Some("aplay"), "idle"),
            stream(7, json!(null), None, "running"),
            stream(8, json!("\u{200b}"), Some("\u{202e}"), "idle"),
        ]));
        let apps = apps(&graph);
        let found: Vec<(&str, &str, Vec<u32>)> = apps
            .iter()
            .map(|app| (app.key.as_str(), app.display.as_str(), app.streams.clone()))
            .collect();
        assert_eq!(
            found,
            [
                ("\u{202e}", UNNAMED, vec![8]),
                ("aplay", "aplay", vec![6]),
                ("chrome", "Chromium", vec![3]),
                ("Discord", "Chromium", vec![1]),
                ("slack", "Chromium", vec![2]),
                ("mpv", "mpv", vec![4, 5]),
            ]
        );
    }

    #[test]
    fn a_target_names_an_app_by_its_binary_or_its_name_whole_and_ignoring_case() {
        let graph = parse(&dump(&[
            stream(1, json!("Firefox"), Some("firefox-bin"), "idle"),
            stream(2, json!("Spotify"), Some("spotify"), "idle"),
            stream(3, json!("spotify"), Some("other"), "idle"),
            stream(4, json!("Music Player"), None, "idle"),
        ]));
        let apps = apps(&graph);
        let named = |target: &str| find_app(&apps, target).map(|at| apps[at].key.as_str());
        assert_eq!(named("firefox-bin"), Some("firefox-bin"));
        assert_eq!(named("FIREFOX"), Some("firefox-bin"));
        assert_eq!(named("music player"), Some("Music Player"));
        // A binary that matches wins over a name that does.
        assert_eq!(named("Spotify"), Some("spotify"));
        assert_eq!(named("fire"), None);
        assert_eq!(named("firefox "), None);
        assert_eq!(named(""), None);
    }

    #[test]
    fn the_default_app_is_one_that_is_playing_never_one_only_open() {
        let graph = parse(&with(&[
            stream(201, json!("Anki"), Some("anki"), "idle"),
            stream(202, json!("Spotify"), Some("spotify"), "running"),
            stream(203, json!("Zoom"), Some("zoom"), "idle"),
        ]));
        let apps = apps(&graph);
        let picked = default_app(&apps).map(|at| apps[at].key.as_str());
        assert_eq!(picked, Some("spotify"));
        assert_eq!(pick_app(&apps, None), default_app(&apps));
        assert_eq!(
            pick_app(&apps, Some("zoom")).map(|at| apps[at].key.as_str()),
            Some("zoom")
        );
        assert_eq!(pick_app(&apps, Some("vlc")), None);
    }

    #[test]
    fn next_app_goes_round_from_the_current_one() {
        let graph = parse(&dump(&[
            stream(1, json!("a"), Some("a"), "idle"),
            stream(2, json!("b"), Some("b"), "running"),
            stream(3, json!("c"), Some("c"), "idle"),
        ]));
        let apps = apps(&graph);
        let next = |current: Option<&str>| next_app(&apps, current).map(|at| apps[at].key.as_str());
        assert_eq!(next(Some("a")), Some("b"));
        assert_eq!(next(Some("C")), Some("a"));
        // With none yet, the knob was on the one playing.
        assert_eq!(next(None), Some("c"));
        // One that has gone starts over.
        assert_eq!(next(Some("gone")), Some("a"));
        assert_eq!(next_app(&[], None), None);

        let graph = parse(&dump(&[stream(1, json!("a"), Some("a"), "idle")]));
        let alone = super::apps(&graph);
        // Nothing playing, and nothing to go on from: the first.
        assert_eq!(next_app(&alone, None), Some(0));
        assert_eq!(next_app(&alone, Some("a")), Some(0));
    }

    #[test]
    fn streams_per_app_and_apps_are_both_capped() {
        let mut objects: Vec<Value> = (1..=20)
            .map(|id| {
                stream(
                    id,
                    json!("Browser"),
                    Some("browser"),
                    if id == 20 { "running" } else { "idle" },
                )
            })
            .collect();
        // Named to come after the browser, so it is not the one cut.
        objects.extend((100..140).map(|id| stream(id, json!(format!("zapp {id}")), None, "idle")));
        let graph = parse(&dump(&objects));
        let apps = apps(&graph);
        assert_eq!(apps.len(), MAX_APPS);
        let browser = &apps[find_app(&apps, "browser").unwrap()];
        assert_eq!(browser.streams.len(), MAX_STREAMS);
        // The one playing is among those kept, and first.
        assert_eq!(browser.streams[0], 20);
    }

    #[test]
    fn targets_offer_every_sink_and_every_app() {
        let graph = parse(&with(&[stream(
            201,
            json!("Spotify"),
            Some("spotify"),
            "running",
        )]));
        let targets = targets(&graph);
        let outputs: Vec<(&str, bool, bool)> = targets
            .outputs
            .iter()
            .map(|output| (output.display.as_str(), output.usable, output.default))
            .collect();
        assert_eq!(
            outputs,
            [
                ("Headphones", false, false),
                ("Speaker", true, true),
                ("HDMI 1", false, false),
                ("HDMI 2", false, false),
                ("HDMI 3", false, false),
            ]
        );
        assert_eq!(targets.outputs[1].name, SPEAKER_NAME);
        assert_eq!(targets.outputs[1].nick, "Speaker");
        assert_eq!(
            targets.apps,
            [
                AppTarget {
                    app: "sd_dummy".into(),
                    display: "speech-dispatcher-dummy".into(),
                    binary: "sd_dummy".into(),
                    running: false,
                },
                AppTarget {
                    app: "spotify".into(),
                    display: "Spotify".into(),
                    binary: "spotify".into(),
                    running: true,
                },
            ]
        );
    }
}
