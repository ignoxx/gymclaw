# Workout Guide assets

Source: https://github.com/bryllim/workout-guide
Commit: aac599224bb9780305239607ef98540b7e0ce389

GymClaw bundles first-pose artwork for all 302 upstream exercises, plus exercises the catalog lacks,
drawn in the same style (see "Local additions").
Upstream SVG files are unmodified. Dashboard CSS inverts their white artwork on light backgrounds.
PNG files are 1024 px renders of the SVGs with an opaque black background for contrast in Telegram
chat themes (`scripts/render-illustrations`). Chat captions carry no credit line; attribution lives here and in the README.
GymClaw made these presentation changes. Adapted PNGs remain CC BY-SA 4.0.
The upstream manifest retains per-frame attribution and recorded Everkinetic adaptations.
Only frame 1 is bundled. Frames 2 and 3 in upstream metadata are not available locally.

## Local additions

Not part of upstream. Each was drawn by an image model with Workout Guide frames as style
references, matched to the catalog's line width and size, and traced with Workout Guide's own
vectorizer (`scripts/new-illustration`, docs/illustrations.md). They adapt CC BY-SA 4.0 art, so
they are CC BY-SA 4.0 too. Their manifest entries credit GymClaw with Workout Guide as the source.

- `horizontal-leg-press`

Visual assets are CC BY-SA 4.0. Upstream code and metadata are MIT.
This does not choose a license for GymClaw's own code.

Bryl Lim: https://bryllim.com
Everkinetic: https://github.com/everkinetic/data
License: https://creativecommons.org/licenses/by-sa/4.0/
