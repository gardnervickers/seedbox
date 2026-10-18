# Control image for the PostgreSQL 18.6 postmaster hang: the official 18.6 image
# with its server binary rebuilt from the 18.6 sources plus upstream commit
# 6a1759a02a ("Fix postmaster failing to exit when startup crashes during crash
# restart", REL_18_STABLE, after 18.6). Everything else (initdb, libraries,
# configuration) is the official image's; the version string stays the image's,
# because initdb refuses a server binary whose version string differs.
FROM docker.io/library/postgres:18.6@sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336 AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential bison flex curl ca-certificates bzip2 patch pkg-config \
      libicu-dev libssl-dev liblz4-dev libzstd-dev liburing-dev libnuma-dev \
      libsystemd-dev libxml2-dev uuid-dev libkrb5-dev libldap-dev libpam0g-dev libreadline-dev zlib1g-dev
WORKDIR /src
RUN curl -sfLO https://ftp.postgresql.org/pub/source/v18.6/postgresql-18.6.tar.bz2 \
 && echo "555610c24d53e4316da5b7d3fc25c279d96856d5e0e23ee308c328c5fa881d9f  postgresql-18.6.tar.bz2" | sha256sum -c \
 && tar xjf postgresql-18.6.tar.bz2
COPY 6a1759a02a.patch /src/
RUN cd postgresql-18.6 && patch -p1 < ../6a1759a02a.patch \
 && ./configure --prefix=/usr --bindir=/usr/lib/postgresql/18/bin --datadir=/usr/share/postgresql/18 \
      --libdir=/usr/lib/aarch64-linux-gnu/ --libexecdir=/usr/lib/postgresql/ --sysconfdir=/etc/postgresql-common \
      --includedir=/usr/include/postgresql/ --with-extra-version=" (Debian 18.6-1.pgdg13+2)" \
      --with-icu --with-openssl --with-lz4 --with-zstd --with-liburing --with-libnuma --with-systemd \
      --with-libxml --with-uuid=e2fs --with-gssapi --with-ldap --with-pam --with-pgport=5432 \
      --with-system-tzdata=/usr/share/zoneinfo CFLAGS="-g -O2" \
 && make -j4
FROM docker.io/library/postgres:18.6@sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336
COPY --from=build /src/postgresql-18.6/src/backend/postgres /usr/lib/postgresql/18/bin/postgres
