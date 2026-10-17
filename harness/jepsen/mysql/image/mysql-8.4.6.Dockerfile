# The official MySQL 8.4.6 image (Oracle Linux 9, arm64) without its /dev entries or
# MySQL Shell. The Oracle Linux base layer ships device nodes (dev/null,
# dev/console, ...) and a FIFO (dev/initctl), and a Seedbox root filesystem may
# contain neither; the guest provides /dev itself. Every other top-level entry is
# copied unchanged (ownership and modes included), with the image's environment,
# entrypoint and command.
FROM docker.io/library/mysql:8.4.6@sha256:6752fee1ed70df146fe22d5d1656deee79463f780a24cb7e67c807f2f07d7dc5 AS source
# MySQL Shell (and the Python it bundles) and documentation are most of the image;
# the root filesystem lives in guest RAM, so leave them out.
RUN rm -rf /usr/lib/mysqlsh /usr/libexec/mysqlsh /usr/bin/mysqlsh /usr/bin/mysql-secret-store-* \
        /usr/lib/python3.9 /usr/lib64/python3.9 /usr/share/doc /usr/share/man
# The top-level symlinks and empty mount points, as a tree to copy.
RUN mkdir /skeleton && cd /skeleton \
    && ln -s usr/bin bin && ln -s usr/sbin sbin && ln -s usr/lib lib && ln -s usr/lib64 lib64 \
    && ln -s usr/local/bin/docker-entrypoint.sh entrypoint.sh \
    && mkdir dev proc sys mnt media srv boot tmp && chmod 1777 tmp

FROM scratch
COPY --from=source /skeleton/ /
COPY --from=source /usr /usr
COPY --from=source /etc /etc
COPY --from=source /var /var
COPY --from=source /run /run
COPY --from=source /root /root
COPY --from=source /home /home
COPY --from=source /opt /opt
COPY --from=source /docker-entrypoint-initdb.d /docker-entrypoint-initdb.d
ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    MYSQL_MAJOR=8.4 MYSQL_VERSION=8.4.6-1.el9
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["mysqld"]
