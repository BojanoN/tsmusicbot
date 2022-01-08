FROM debian:buster-slim

RUN apt update && apt install -y ffmpeg wget python
RUN wget -L https://yt-dl.org/downloads/latest/youtube-dl -O /usr/local/bin/youtube-dl && chmod +x  /usr/local/bin/youtube-dl

RUN useradd -ms  /bin/false bot

WORKDIR /bot
RUN chown bot /bot
USER bot
COPY config/config.json .

RUN wget "https://github.com/BojanoN/tsmusicbot/releases/download/v0.1/tsmusicbot-0.1" -O tsmusicbot && chmod +x tsmusicbot

CMD ["./tsmusicbot"]