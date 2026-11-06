# nix-build-substitutable

## Slop warning
This was vibe coded and I haven't read the code. For the artisanally coded hack MVP, use the following:
```bash
if ! nix_build_output=$(
  nix build .#package --dry-run 2>&1 >/dev/null
); then
  printf "%s" "$nix_build_output"
  exit 1
fi

if printf "%s" "$nix_build_output" \
  | awk '
  /these [0-9]+ paths will be fetched/{exit}
  /these [0-9]+ derivations will be built/{next}
  {print}
  ' | grep '\.drv$' \
  | xargs grep -vlE '\\"preferLocalBuild\\":true|\("preferLocalBuild","1"\)' > build_drvs.txt; \
then
  xargs nix-store --realise < build_drvs.txt
else
  echo "Seemingly no derivations to be built; output from dry-run:"
  cat dry_run_output.txt
fi
```

## General

`nix build` for CI that fills a binary cache. It builds only the derivations a cache client
would actually fetch, and downloads only what those builds need.

Building a NixOS config normally downloads its whole runtime closure, because the final
`toplevel` derivation needs it as input. Clients never fetch `toplevel` from a cache, so a CI job
that changed one systemd unit can spend many gigabytes on nothing. `nix-build-substitutable` skips it.

## Usage

```console
$ nix run github:OWNER/nix-build-substitutable -- .#nixosConfigurations.host.config.system.build.toplevel
```

```
nix-build-substitutable [--skip-prefer-local] [--skip-shell-applications] [--dry-run] [--push-command <CMD>]
                        <INSTALLABLES>... [-- <NIX_ARGS>...]
```

- **stdout** lists the built output paths, one per line.
- **stderr** has the normal Nix output, plus what will be built, skipped and fetched.
- **`--dry-run`** prints what would be built, skipped and fetched, then stops. No derivations are built,
  no paths are fetched for them and nothing is pushed. As with `nix build --dry-run`, evaluating can
  still download flake inputs and write `.drv` files to the store.
- **`--push-command <CMD>`** runs `sh -c CMD` with the output paths on stdin. It doesn't run if
  nothing was built.
- **`<NIX_ARGS>`** go to `nix build`, for example `-- -L --impure`. Only `--option NAME VALUE`
  also reaches the cache query, so pass settings that affect it in that form or in `nix.conf`.
  `--expr` and `--file` aren't supported.

Both of these push to [Attic](https://github.com/zhaofengli/attic):

```console
$ nix-build-substitutable .#pkg | attic push main --stdin
$ nix-build-substitutable --push-command 'attic push main --stdin' .#pkg
```

`attic push` also uploads the runtime closure of those paths, including any skipped derivations
they reference.

## Choosing a store

The tool uses the same store as Nix, however you select it: `-- --store X`, `-- --option store X`,
`store = X` in `nix.conf`, `NIX_CONFIG`, or `NIX_REMOTE`.

A store in a local directory is handled by your own process rather than the Nix daemon. If your
`nix.conf` sets `secret-key-files` to a key only root can read, add
`-- --option secret-key-files ''`.

## What gets built

1. Nix's own `queryMissing`, the computation behind `nix build --dry-run`, lists what would have
   to be built given your configured substituters. It uses the same concurrent lookups and
   narinfo cache as Nix, rather than asking each cache about each path.
2. Derivations a client can never fetch are removed: those with `allowSubstitutes = false`, unless
   `always-allow-substitutes` is on.
3. What's left is built with `nix build`, which downloads only the inputs those builds need.

Uncached inputs of a skipped derivation are still built, because a client needs them to build
that derivation locally.

### `preferLocalBuild`

`preferLocalBuild` alone doesn't stop a client fetching from a cache. It's why `fetchurl` outputs,
which set only that attribute, can be substituted. So by default these derivations are built, which
can put source tarballs in your cache. Use `--skip-prefer-local` to skip them too; clients then
download those sources from upstream if they need them.

### `writeShellApplication`

nixpkgs lets clients fetch these scripts, so they're built by default. A client can build one itself
in seconds, provided it can get ShellCheck, so `--skip-shell-applications` skips them too.

Derivations don't record which builder made them, so this is a best-effort match on an executable
text file under `/bin` with a bash shebang, `allowSubstitutes = true` and `preferLocalBuild = false`.
Scripts it misses just get built, and they're listed under "will build". A hand-written
`writeTextFile` with those same settings is skipped too. Skipped scripts don't get their ShellCheck
run in CI, so a bad one fails on the client.

NixOS's `activate` script sets `preferLocalBuild = true`, so `--skip-prefer-local` skips it instead.

## GitHub Actions

```yaml
- uses: cachix/install-nix-action@v31
- name: Configure attic
  run: |
    nix profile add nixpkgs#attic-client
    attic login main https://cache.example.com "${{ secrets.ATTIC_TOKEN }}"
    attic use main
- name: Build and push
  run: |
    nix run github:OWNER/nix-build-substitutable -- \
      --push-command 'attic push main --stdin' \
      .#nixosConfigurations.host.config.system.build.toplevel
```

## Limitations

- If any build fails, nothing is pushed, even with `--keep-going`. Nix prints no output paths on
  failure.
- Tested on Nix 2.31 and 2.34, with local directory stores and the daemon. Remote stores such as
  `ssh-ng://` are untested. Lix and Determinate Nix are untested.

## Development

```console
$ nix develop
$ cargo test
```

The end-to-end tests use `nix` from `PATH`, in a private store that doesn't touch the system one.
