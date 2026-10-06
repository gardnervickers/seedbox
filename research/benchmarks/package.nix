# Proposed pure package; coordinator wires this to shared pinned nixpkgs.
{ lib, stdenvNoCC, fetchurl, jdk21, makeWrapper }:
let
  lock = builtins.fromJSON (builtins.readFile ./artifacts.lock.json);
  profile = builtins.fromJSON (builtins.readFile ./profile.json);
  jars = map (a: fetchurl { inherit (a) url hash; }) lock.artifacts;
  classpath = lib.concatStringsSep ":" (map toString jars);
in stdenvNoCC.mkDerivation {
  pname = "seedbox-redis-jedis-benchmark";
  version = "0.1.0";
  src = ./src;
  nativeBuildInputs = [ jdk21 makeWrapper ];
  buildPhase = ''
    mkdir -p classes
    javac --release 21 -cp ${lib.escapeShellArg classpath} -d classes main/java/seedbox/benchmark/RedisJedisBenchmark.java
    jar --create --date=2020-01-01T00:00:00Z --file benchmark.jar -C classes .
  '';
  installPhase = ''
    mkdir -p $out/share/seedbox-benchmark $out/bin
    cp benchmark.jar $out/share/seedbox-benchmark/
    cp ${./profile.json} $out/share/seedbox-benchmark/profile.json
    makeWrapper ${jdk21}/bin/java $out/bin/seedbox-redis-jedis-benchmark \
      --add-flags ${lib.escapeShellArg (lib.concatStringsSep " " profile.jvm.flags)} \
      --add-flags "-cp $out/share/seedbox-benchmark/benchmark.jar:${classpath} seedbox.benchmark.RedisJedisBenchmark"
  '';
}
