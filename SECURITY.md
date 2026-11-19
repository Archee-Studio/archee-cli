# Security

Report a vulnerability by email to team@archee.studio, not in a public issue. We reply within a few working days.

## What `archee connect` can reach

`connect` is not a sandbox. Commands Archee runs in the served folder execute as your operating system user: a script can read any file you can read, including a saved `archee login`, and use the network. The file tools stay inside the folder; shell commands do not. Run `archee logout` before connecting if that matters, and only connect folders you are willing to share.
