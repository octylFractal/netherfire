netherfire
==========

A Minecraft modpack automation tool. Takes a modpack configuration and spits out a working modpack.

## Support
Supported inputs are CurseForge, Modrinth, and arbitrary override directories for common, client, and server.

Supported outputs are CurseForge modpacks, Modrinth modpacks, or a server directory.

## How to Use

First, create a directory to act as the "source" for your modpack.

Then add a `config.toml` file for the configuration of the general properties of the modpack. It should have the
following properties:

- `name`: The name of the modpack.
- `description`: The description of the modpack.
- `author`: The author of the modpack.
- `version`: The version of the modpack.
- `minecraft_version`: The version of Minecraft the modpack is for.
- `modloader.id`: The ID of the modloader to use. `forge`, `neoforge`, `fabric`, or `quilt`.
- `modloader.version`: The version of the modloader to use.

Add a `mods.toml` file for the configuration of the mods in the modpack. Mods from any source may be included in any
pack, but they may be downloaded and included as an override, increasing the size of the pack.

There are two sections in the `mods.toml`: `mods.curseforge` and `mods.modrinth`. Each section contains a list of
mods to include from the respective mod site. CurseForge mods use an `i32` project and version ID, while Modrinth mods
use a `String` project and version ID. Do not use the slug for Modrinth mods, as it is subject to change and will
introduce errors.

Each section contains a set of mappings from an arbitrary identifier to the `project_id`, `version_id`, and requirement
information (`client` and `server`). If a mod includes bad dependency information, you can also exclude the bad
dependency via `ignored_dependencies`.

As an example, here is a `mods.toml` for a modpack that includes the Fabric API and JEI for 1.20.1 from both CurseForge
and Modrinth:

```toml
[mods.curseforge]
fabric-api = { project_id = 306612, version_id = 4787692 }
jei = { project_id = 238222, version_id = 4712867, server = "optional" }

[mods.modrinth]
fabric-api = { project_id = "P7dR8mSH", version_id = "tFw0iWAk" }
# Note that there is no explicit `server` marking here, as the correct value is inherited from the Modrinth metadata.
jei = { project_id = "u6dRKJwZ", version_id = "lIRFslED" }
```

Optionally, also add files to `overrides/`, `client-overrides/`, and `server-overrides/` to include any files you want
in the distributions directly. If you want mods not from CurseForge or Modrinth, you can also add them to a `mods/`
directory in any of the override directories.

Next, run `netherfire <source directory>`. This verifies that the configuration loads and is valid.

Check `netherfire --help` and pick the distributions you want. Note that the Modrinth pack also includes the server
mods and files for use with tools like [modrinth-install](https://github.com/nothub/mrpack-install). Each output option
takes a directory to store the output in.

Run the `netherfire` command again with the options you want. This will download the mods and create the
distribution(s).

And that's it! You now have working packs to distribute to your friends or upload to CurseForge or Modrinth.
