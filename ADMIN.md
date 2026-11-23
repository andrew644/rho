# Admin

## Discord: give the bot permission to react

rho now puts a 👀 reaction on every prompt it accepts and shows "typing" in
the channel while the answer is being written. Reactions are a channel
permission, not a gateway intent, so nothing in the developer portal is
required — but the bot needs both permissions below in each server, or every
reaction is refused with `403 Missing Permissions` (rho reports the first
refusal once and keeps working without it). Direct messages need neither.

A `405 Method Not Allowed` was rho's bug, not a permission: reactions are a
`PUT` to `/channels/{id}/messages/{id}/reactions/{emoji}/@me`.

Re-invite the bot with the extra bits, or add them to the existing role:

```
https://discord.com/api/oauth2/authorize?client_id=<APP_ID>&scope=bot&permissions=68672
```

`68672` = Read Message History (65536) + Send Messages (2048) + View
Channel (1024) + Add Reactions (64). Discord requires Read Message History
for creating a reaction, not just Add Reactions. Typing needs no permission
at all.
