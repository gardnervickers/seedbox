# The official MariaDB 12.1.2 image (Ubuntu 24.04, arm64; Galera provider 26.4.24),
# slimmed: a guest's root filesystem lives in RAM and the boot floor is three times the
# archive plus 128 MiB, so leave out Perl, the backup, Aria and MyISAM tools, iconv
# modules and documentation. Nothing the server, the rsync SST script, mariadb-install-db
# or the client needs is removed. The Ubuntu base has no device nodes in /dev, so no
# scratch copy is needed.
FROM docker.io/library/mariadb@sha256:21407e376f311cca4266d9aa94ddedcf5023fd082bc836e3710e8deff9f311c6
RUN rm -rf /usr/share/perl /usr/share/perl5 /usr/lib/aarch64-linux-gnu/perl \
        /usr/lib/aarch64-linux-gnu/perl-base /usr/lib/aarch64-linux-gnu/perl5 \
        /usr/lib/aarch64-linux-gnu/libperl.so* /usr/bin/perl /usr/bin/perl5* \
        /usr/bin/mariadb-backup /usr/bin/mariabackup /usr/bin/mbstream \
        /usr/bin/aria_* /usr/bin/myisam* /usr/bin/mariadb-binlog /usr/bin/mysqlbinlog \
        /usr/bin/mariadb-conv /usr/bin/mariadb-import /usr/bin/mariadb-slap \
        /usr/bin/mariadb-test* /usr/bin/mariadb-client-test* /usr/bin/innotop /usr/bin/mytop \
        /usr/lib/aarch64-linux-gnu/gconv /usr/share/doc /usr/share/man /usr/share/info \
        /var/lib/apt/lists/* /var/cache/*
