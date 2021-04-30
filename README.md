# tsmusicbot
A simple TeamSpeak3 music bot built using [tsclientlib](https://github.com/ReSpeak/tsclientlib). Uses `ffmpeg` and `youtube-dl` for audio download and manipulation.

## Requirements
A Linux-based OS, `ffmpeg` and `youtube-dl`.

## Overview
### Getting started 
After building or downloading the precompiled program, create a `config.json` file in the current directory and fill out the desired configuration parameters.
Proceed to execute the program afterwards.

### Supported commands
* `!yt <media_url>` - queues the requested url for playback
* `!stop` - stops playback of the current song
* `!volume <float value in [0, 1]>` - adjusts playback volume

