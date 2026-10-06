dev/dev.sh up # build, start server, mint pairing code, enroll + start client
dev/dev.sh status # GET /clients: shows enrolled clients and whether each is connected
dev/dev.sh logs
dev/dev.sh restart-server # test client reconnect/backoff
dev/dev.sh native # run the client from source (cargo) against the dev server
dev/dev.sh down # stop, keep data
dev/dev.sh reset # stop and wipe dev DB + identity
