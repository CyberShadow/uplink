# vercel-labs/emulate, built from source with our patches (patches/, exported
# from a local branch; see docs/SPEC.md §15.10). Used as the Gmail and Google
# OAuth fake in integration tests.
{ lib, stdenv, fetchFromGitHub, fetchPnpmDeps, pnpmConfigHook, pnpm_11, nodejs_24, makeWrapper }:

stdenv.mkDerivation (finalAttrs: {
  pname = "emulate";
  version = "0.13.0-unstable-2026-11-06";

  src = fetchFromGitHub {
    owner = "vercel-labs";
    repo = "emulate";
    rev = "617916f656125c423d13cfbd00b0923b7d0c38f6";
    hash = "sha256-OTXpFutEw2qY5eienLBCshgyL/UMd0BZpJSFeUZ5KYk=";
  };

  patches = lib.filesystem.listFilesRecursive ./patches;

  # Only the CLI and the workspace packages it bundles.
  pnpmWorkspaces = [ "emulate..." ];

  pnpmDeps = fetchPnpmDeps {
    inherit (finalAttrs) pname version src pnpmWorkspaces;
    pnpm = pnpm_11;
    fetcherVersion = 3;
    hash = "sha256-0MPpgSreqXmQEZc9a55plvOGTAINIZtYevn+QyOrr+E=";
  };

  nativeBuildInputs = [ nodejs_24 pnpm_11 pnpmConfigHook makeWrapper ];

  buildPhase = ''
    runHook preBuild
    pnpm --filter "emulate..." build
    runHook postBuild
  '';

  # The bundle inlines @emulators/*; only the CLI's own dependencies stay
  # external, so deploy just that package with its production dependencies.
  # Non-legacy deploy reuses the workspace lockfile (legacy deploy re-resolves,
  # which needs the network); it requires injected workspace packages.
  installPhase = ''
    runHook preInstall
    pnpm --filter emulate deploy --prod --offline \
      --config.inject-workspace-packages=true $out/lib/emulate
    rm $out/lib/emulate/{pnpm-lock.yaml,pnpm-workspace.yaml}
    makeWrapper ${nodejs_24}/bin/node $out/bin/emulate --add-flags $out/lib/emulate/dist/index.js
    runHook postInstall
  '';

  meta.mainProgram = "emulate";
})
