# HTTPS-only forward proxy: GymClaw's single route to the internet.
FROM alpine:3.22
RUN apk add --no-cache squid
USER squid
COPY squid.conf /etc/squid/squid.conf
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
