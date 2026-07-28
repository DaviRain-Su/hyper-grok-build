# Nix flake for the Hyper (`hyper`) terminal AI coding agent.
#
# This flake builds the `hyper` binary (the composition-root crate
# `xai-grok-pager-bin`) and provides a dev shell with all build-time
# tooling. It is written in the same style as an in-tree nixpkgs package
# expression so that the `package` output below can be lifted almost
# verbatim into `pkgs/by-name/hy/hyper-grok-build/package.nix` when
# contributing upstream — only the `import <nixpkgs>` / `lib` plumbing
# differs.
#
# Usage:
#   nix build .#hyper-grok-build        # build the `hyper` binary
#   nix run   .#hyper-grok-build -- --version
#   nix develop                          # rust + protoc + cmake + git shell
#
# The first `nix build` will fail on `cargoLock.outputHashes` for the
# `async-openai` git fork and print the correct SRI hash — copy it into
# `outputHashes` below and rebuild. See:
# https://nixos.org/manual/nixpkgs/unstable/#buildrustpackage
{
  description = "Hyper — unofficial multi-provider community build of Grok Build (terminal AI coding agent)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # The `hyper` binary ships from the `xai-grok-pager-bin` crate; the
      # workspace root has no `[package]`, so we target the crate directly.
      mainCrate = "xai-grok-pager-bin";

      forAllSystems = nixpkgs.lib.genAttrs nixpkgs.lib.systems.flakeExposed;

      nixpkgsFor =
        system:
        import nixpkgs {
          inherit system;
          # No overlays so this expression stays portable into a plain
          # nixpkgs tree (no flake-only machinery).
        };
    in
    {
      packages = forAllSystems (
        localSystem:
        let
          pkgs = nixpkgsFor localSystem;
        in
        {
          # Default output: the `hyper` binary, exposed under the
          # non-conflicting attribute name `hyper-grok-build` (the `hyper`
          # attribute is already taken in nixpkgs by the Hyper terminal).
          default = self.packages.${localSystem}.hyper-grok-build;

          hyper-grok-build = pkgs.rustPlatform.buildRustPackage (finalAttrs: {
            pname = "hyper-grok-build";
            # Root `VERSION` file is the lockstep client version, identical
            # to `crates/codegen/xai-grok-pager-bin/Cargo.toml`'s `version`.
            version = "0.2.113";

            # Whole-workspace source — vendored `third_party/*` path deps and
            # `.cargo/config.toml` (linker hardening, jemalloc page size,
            # `LIBOPUS_NO_PKG=1`, `CMAKE_POLICY_VERSION_MINIMUM`) live at the
            # root and are needed by the build.
            #
            # NOTE(stage-2: nixpkgs PR): replace this with `fetchFromGitHub`
            # pointing at the release tag, e.g.:
            #   src = fetchFromGitHub {
            #     owner = "DaviRain-Su";
            #     repo = "hyper-grok-build";
            #     rev = "v${finalAttrs.version}";
            #     hash = "sha256-...";
            #   };
            src = ./.;

            # Lock the dependency graph to the committed `Cargo.lock`.
            # `outputHashes` pins every git-source crate (two git repos,
            # four crates total). Both repos are public — verified via
            # anonymous `git ls-remote`:
            #   - github.com/our-forks/async-openai @ 95b52eb
            #   - github.com/helix-editor/nucleo     @ 5b74652
            # Keys are `<crate-name>-<version>` exactly as in `Cargo.lock`.
            # Note `async-openai` ships `0.33.1` even though `[patch.crates-io]`
            # requests `0.33.0`. Replace each placeholder SRI hash with the
            # one `nix build` reports on first failure (it lists them one at
            # a time — fix, rebuild, repeat).
            cargoLock = {
              lockFile = ./Cargo.lock;
              outputHashes = {
                # async-openai + async-openai-macros come from the same git repo
                # (our-forks/async-openai @ 95b52eb) → same fetched-output hash.
                "async-openai-0.33.1" = "sha256-pCq9Wo50T6SKlVbZk58v8NrhTi9iwZQ5cErm7uB9+eY=";
                "async-openai-macros-0.1.1" = "sha256-pCq9Wo50T6SKlVbZk58v8NrhTi9iwZQ5cErm7uB9+eY=";
                # nucleo + nucleo-matcher come from the same git repo
                # (helix-editor/nucleo @ 5b74652) → same fetched-output hash.
                "nucleo-0.5.0" = "sha256-ztSgjBI8vhKvrWmpT5K1UoHQRnbbrbEtSnvRkFmhSNc=";
                "nucleo-matcher-0.3.1" = "sha256-ztSgjBI8vhKvrWmpT5K1UoHQRnbbrbEtSnvRkFmhSNc=";
              };
            };

            # Build only the shipped binary crate, not the whole workspace's
            # test/example/bin targets.
            cargoBuildFlags = [
              "-p"
              mainCrate
            ];

            # `protoc` — prost-build compiles `.proto` files.
            # `cmake` — present for any cmake-based `-sys` crate; audiopus_sys
            #           pulls it but we bypass its bundled build (see below).
            # `git`  — build.rs reads `git rev-parse --short HEAD` (falls
            #           back to "unknown" if absent, so this is optional but
            #           keeps VERSION_WITH_COMMIT accurate).
            # `pkg-config` — probes in some `-sys` crates.
            nativeBuildInputs = [
              pkgs.protobuf
              pkgs.cmake
              pkgs.git
              pkgs.pkg-config
            ];

            # `audiopus_sys` (the Opus FFI used by the `/live` voice
            # subsystem) has an upstream bug: its build.rs hardcodes
            # `cargo:rustc-link-search=.../lib`, but the bundled cmake
            # install puts `libopus.a` in `lib64` on 64-bit targets, so the
            # bundled build fails with "could not find native static library
            # `opus`". `.cargo/config.toml` sets `LIBOPUS_NO_PKG=1` (skip
            # pkg-config), and build.rs checks `LIBOPUS_LIB_DIR` *before*
            # falling back to the bundled cmake build. Pointing that env at
            # nixpkgs' static `opus` (`lib/libopus.a`) makes it link the
            # system static lib instead of the broken bundled build — same
            # Opus, static, ABI-equivalent. `opus` in buildInputs puts
            # `include/` on the compile search path for the FFI headers.
            buildInputs = [ pkgs.opus ];
            env.LIBOPUS_LIB_DIR = pkgs.lib.getOutput "out" pkgs.opus;

            # jemalloc and dav1d are still bundled via their `-sys` crates
            # (no system-lib bug), so nothing extra needed for those.

            # `.cargo/config.toml` sets per-target rustflags + env (hardening,
            # jemalloc page size, LIBOPUS_NO_PKG); buildRustPackage respects
            # it automatically — single source of truth, not duplicated here.

            # Workspace tests are heavy; gate separately in CI, not in the
            # package build.
            doCheck = false;

            meta = with pkgs.lib; {
              description = "Unofficial multi-provider community build of Grok Build — terminal AI coding agent";
              homepage = "https://github.com/DaviRain-Su/hyper-grok-build";
              license = licenses.asl20;
              # Single shipped binary `hyper`; required for `nix run` to
              # resolve to the right executable.
              mainProgram = "hyper";
              # Linux glibc targets first (matches the repo's release matrix).
              # macOS works upstream but is not exercised here.
              platforms = platforms.linux;
              # TODO(stage-2: nixpkgs PR): add yourself to
              # `maintainers/maintainer-list.nix` and reference the handle.
              maintainers = [ ];
            };
          });
        }
      );

      devShells = forAllSystems (
        localSystem:
        let
          pkgs = nixpkgsFor localSystem;
          # Use the top-level `cargo`/`rustc` (the same toolchain the package
          # build uses) rather than `rustPlatform.rust.*`, which is
          # deprecated. `rustPlatform.rustLibSrc` is for rust-analyzer.
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustPlatform.rustLibSrc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.protobuf
              pkgs.cmake
              pkgs.git
              pkgs.pkg-config
            ];
          };
        }
      );

      # Convenience app so `nix run .#hyper-grok-build` works without
      # remembering the binary is named `hyper` (mainProgram also handles
      # this, but an explicit app is friendlier for newcomers).
      apps = forAllSystems (localSystem: {
        default = {
          type = "app";
          program = "${self.packages.${localSystem}.hyper-grok-build}/bin/hyper";
        };
      });

      formatter = forAllSystems (localSystem: (nixpkgsFor localSystem).nixfmt-rfc-style);
    };
}
