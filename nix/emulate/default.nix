# vercel-labs/emulate (https://github.com/vercel-labs/emulate), from its npm
# release: stateful fakes of provider APIs (Gmail REST + Google OAuth), used
# by the integration tests. The release bundles all emulators into
# `dist/`; package-lock.json pins it and its three runtime dependencies.
{ buildNpmPackage, nodejs_24, makeWrapper }:

buildNpmPackage {
  pname = "emulate";
  version = "0.13.0";
  src = ./.;
  nodejs = nodejs_24;
  npmDepsHash = "sha256-xdnLjJFnOZZxc6ldC2vIUIBjsN+eg0Zptb7OrT1Sht8=";
  dontNpmBuild = true;
  nativeBuildInputs = [ makeWrapper ];
  postInstall = ''
    makeWrapper ${nodejs_24}/bin/node $out/bin/emulate \
      --add-flags $out/lib/node_modules/uplink-test-emulate/node_modules/emulate/dist/index.js
  '';
}
