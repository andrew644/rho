# Admin requests

## Enable the Message Content Intent for the rho bot (needed now)

rho now requests the privileged `GUILD_MESSAGE_CONTENT` intent so that it can
read text posted in server channels. Until it is switched on the gateway
closes `IDENTIFY` with `4013 Disallowed intents` and rho falls back to
DM-only (it warns and reconnects without the intent):

1. <https://discord.com/developers/applications> → the rho application → **Bot**
2. Under **Privileged Gateway Intents**, turn on **Message Content Intent** and
   Save.

Leave *Server Members Intent* and *Presence Intent* off — rho does not ask for
them.

## Force push rewritten history (needed now)

The `target/` directory was accidentally committed in the first 7 commits.
History has been rewritten locally with `git filter-repo` (verified: no
commit contains `target/` anymore, repo pack is ~49 KB). I don't have SSH
access to GitHub, so please run:

    cd ~/repos/rho
    git push --force origin main

Note: this overwrites `origin/main` (old tip `11435cc`). If anyone else has
cloned the old history, they should re-clone or run
`git fetch && git reset --hard origin/main`.
