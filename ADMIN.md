# Admin requests

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
