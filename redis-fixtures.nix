{ pkgs }:
pkgs.pkgsStatic.stdenv.mkDerivation {
  pname = "seedbox-redis-fixtures";
  version = "7.2.7";
  nativeBuildInputs = [ pkgs.binutils pkgs.which pkgs.pkg-config ];
  src = pkgs.fetchurl {
    url = "https://download.redis.io/releases/redis-7.2.7.tar.gz";
    sha256 = "72c081e3b8cfae7144273d26d76736f08319000af46c01515cad5d29765cead5";
  };
  buildPhase = ''
    runHook preBuild
    make -j"$NIX_BUILD_CORES" MALLOC=libc USE_SYSTEMD=no BUILD_TLS=no OPT=-O2 LDFLAGS=-static
    $CC -O2 -static -I deps/hiredis ${./guests/redis_ttl.c} deps/hiredis/libhiredis.a -o redis-ttl-client
    runHook postBuild
  '';
  installPhase = ''
    mkdir -p "$out/bin"
    cp src/redis-server src/redis-cli "$out/bin/"
    cp redis-ttl-client "$out/bin/"
  '';
}
