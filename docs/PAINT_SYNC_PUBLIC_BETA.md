# Paint Sync Public Beta

## What “public” means

Anyone can join the AMX paint-sync beta from the MXB App without asking for an invite code.
The service works on community servers, including CBR-hosted servers; the server owner does
not need to install a server agent for paint sync.

There is still one required download: each **viewer** needs the MXB App/FrostMod client on
their own Windows PC. MX Bikes does not send custom paint files through the game server, so
a website or server-only mod cannot make them appear inside another player's game.

## Solo viewer mode

You can run the app by yourself and join a normal MX Bikes server. On Play, the app:

1. Reads the joined server and your GUID from FrostMod.
2. Announces that you are present on that server.
3. Downloads the capped catalog of recently published riders, plus the exact live roster
   when other beta clients are present.
4. Installs the missing `.pnt` files and asks FrostMod to refresh them.

The other rider does not need the app open during the race. They do need to have published
their look at least once. If they have never joined paint sync, there is no paint file for
your client to download.

## Model limitation

Paint sync distributes `.pnt` paint files only. It does not redistribute custom bike,
helmet, rider, boot, or other 3D model packages.

For a synced paint to render, the viewer must already have the matching underlying model
installed. For example, a paint made for a custom helmet will not render on a PC that does
not have that helmet model. Redistributing third-party models requires the creator's
permission and is outside this beta.

## First-time setup

1. Install the current public-beta MXB App build.
2. Install the matching `frostmod.dlo` in the MX Bikes `plugins` folder.
3. Open **Settings → Experimental** and enable **Servers and paint sync**.
4. Open **Servers**, choose the exact MX Bikes profile you ride as, and click
   **Join public beta**.
5. Click **Publish now** once. Future changes publish automatically.
6. Start MX Bikes through the app and join any server. Paint sync runs automatically.

## Reading the status panel

- **Your look is published**: your `.pnt` files are available to other beta users.
- **You have the paints of _n_ riders**: this PC fetched published paints for _n_ riders.
- **Identified by GUID**: the account is tied to the stable MX Bikes identity rather than
  only the displayed rider name.
- **0 riders**: nobody currently matched both the server session and a published account.

## Expected test result

Test bike livery, rider kit, helmet, and boots separately. For each item, confirm both PCs
have the same underlying custom model before treating a missing paint as a sync failure.
Record the rider names, bike IDs, model names, server name, and which individual slots did
or did not render.

## Safety and scope

The service accepts validated `.pnt` files only, hashes and de-duplicates them, rejects unsafe
paths, and has a public-beta account ceiling. Public enrollment is temporary; a production
release should use a verified identity such as Steam sign-in plus rate limits and a report/
takedown process.
