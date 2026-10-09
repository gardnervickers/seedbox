FROM scratch
COPY client /client
ENTRYPOINT ["/client"]
