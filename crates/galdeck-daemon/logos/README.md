# App logos

The logos behind the `logo-*` icon names (see `src/logos.rs`), one SVG per
app, named as the icon name has it without `logo-`.

They are from [Simple Icons](https://simpleicons.org) 16.32.0, copied
unchanged from the npm package's `icons/` directory. Simple Icons publishes
its collection under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/),
but some logos in it carry a licence of their own (GPL, CC-BY-SA, MPL and
others), listed in the package's `data/simple-icons.json`. Only logos with no
`license` entry there are included, so nothing here asks for more than CC0.

CC0 covers copyright only. Every logo is a trademark of its owner, and
including it here says nothing about that owner endorsing galdeck. Showing a
logo on a key that opens the app is what it is here for.

## Adding one

1. Find the icon on simpleicons.org and check its entry in
   `data/simple-icons.json` has no `license` field.
2. Copy `icons/<slug>.svg` here as `<name>.svg`, where `<name>` is lowercase
   words joined by dashes.
3. Add a `logo!` line for it to `LOGOS` in `src/logos.rs`, with the title,
   the entry's `hex` and a category, next to the others in that category.

`cargo test --test logos` checks each file is a single path and draws.
