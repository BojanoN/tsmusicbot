FROM rust:alpine as builder

RUN apk add --update --no-cache musl-dev pkgconfig openssl-dev opus-dev

WORKDIR /usr/src/tsmusicbot
COPY . .

RUN cargo install --path .

FROM alpine:latest as final

# Switch to root
USER root
RUN apk add --update --no-cache ffmpeg youtube-dl

# Set user and group
ARG user=bot
ARG group=bot
ARG uid=2000

RUN adduser --uid=${uid} --disabled-password --gecos="" ${user}

USER ${uid}:${uid}
WORKDIR $HOME

COPY --from=builder /usr/local/cargo/bin/tsmusicbot /usr/local/bin/tsmusicbot
COPY ./config.json.default /opt/tsmusicbot/config.json

ENTRYPOINT ["tsmusicbot"]