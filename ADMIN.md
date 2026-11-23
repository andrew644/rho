# Admin

## Discord: give the bot permission to react

rho now puts a 👀 reaction on every prompt it accepts and shows "typing" in the
channel while the answer is being written. Reactions are a channel permission,
not a gateway intent, so nothing in the developer portal is required — but the
bot needs the permission in each server, or every reaction is refused with
`403 Missing Permissions` (rho reports the first refusal once and keeps working
without it).

Re-invite the bot with the extra bit, or add it to the existing role:

```
https://discord.com/api/oauth2/authorize?client_id=<APP_ID>&scope=bot&permissions=3136
```

`3136` = View Channel (1024) + Send Messages (2048) + Add Reactions (64).
Typing needs no permission at all.
