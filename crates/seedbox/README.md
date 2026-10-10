# Seedbox executable

The CLI is the first client of the simulation server. This package also contains
the private retained worker entrypoint; installation still ships one executable.
Application configuration, preparation and orchestration live in
`seedbox-application`; public resources and transports have separate crates.
