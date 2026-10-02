# Reproducing late video

This page reproduces a sender whose video reaches the plugin later than its audio (issues #33 and #34) on a real OBS with the plugin loaded, and measures the lip sync that comes out.

The bug class is arrival skew. The sender stamps audio and video correctly at capture, but the video packets of an instant arrive after the audio packets of the same instant. A phone with video stabilisation on delivers its video a second or more late. pocketSRT queues its audio 300 to 700 ms early. Shifting timestamps with `ffmpeg -itsoffset` does not reproduce this: it changes what the sender stamped, and the plugin plays what the sender stamped. The relay in `crates/irl-core/examples/skew-relay.rs` keeps the stamps and delays the delivery instead. It forwards MPEG-TS over UDP, holds every TS packet of the video PID for a configurable time, and passes audio and tables through at once.

## What you need

- OBS with the plugin build under test.
- ffmpeg with libx264 and the `lavfi` device.
- The relay, built with `cargo build --release -p irl-core --example skew-relay`. It lands in `target/release/examples/skew-relay`.

## Set up the rig

1. In OBS, add an IRL Source with the URL `udp://127.0.0.1:9001`. A new source fits itself to the canvas; keep it that way, because the flash detection below reads the whole frame. If OBS runs on another machine, use `udp://0.0.0.0:9001` and point the relay's `--to` at that machine.
2. Mute every other audio source in the scene.
3. Start the relay with no delay:

   ```shell
   target/release/examples/skew-relay --listen 127.0.0.1:9000 --to 127.0.0.1:9001
   ```

4. Start the test source:

   ```shell
   ffmpeg -re \
     -f lavfi -i "testsrc2=size=1280x720:rate=30,drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='lt(mod(t,1),0.04)'" \
     -f lavfi -i "aevalsrc='0.05*sin(2*PI*440*t)+0.5*sin(2*PI*1000*t)*lt(mod(t,1),0.04)':s=48000" \
     -c:v libx264 -preset veryfast -tune zerolatency -g 60 -b:v 4M \
     -c:a aac -b:a 128k \
     -f mpegts -pes_payload_size 0 "udp://127.0.0.1:9000?pkt_size=1316"
   ```

   The picture turns white for 40 ms at the start of every second, and a 1 kHz beep plays for the same 40 ms. Both come from the same timeline, so they coincide at capture.

Two details in that command keep the rig honest. The quiet 440 Hz bed under the beep keeps the AAC frames full sized, and `-pes_payload_size 0` sends one audio frame per PES packet. Without them the MPEG-TS muxer packs up to 15 audio frames into one PES, the audio then arrives in bursts, and the bursts read as a skew of a few hundred milliseconds on their own.

## Take a baseline

1. Let the stream run for a minute with the relay at no delay.
2. Read the stats line (see below). Expect `av_skew` around -50 ms, which is the encoder's own mux order, `hold=0ms` and `vdelay` near zero.
3. Measure the lip sync from a recording (see below).

This is the rig's own offset. Subtract it from every later measurement.

## Delay profiles

Stop the relay with Ctrl+C and start it again with the flags of a profile. ffmpeg keeps sending through the restart. The relay starts its clock at the first datagram it receives, so a restart also restarts a ramp or a stall cycle.

| case | relay flags |
| --- | --- |
| pocketSRT: audio 500 ms early, and every 4 s the video stalls for 200 ms and then arrives as a burst | `--delay-ms 500 --stall-every 4 --stall-ms 200` |
| a phone with stabilisation on | `--delay-ms 1650` |
| skew that builds up | `--ramp 0:1000:30` |
| skew that recovers | `--ramp 1000:0:30` |

`--ramp FROM:TO:SECONDS` moves the delay linearly from `FROM` to `TO` milliseconds over `SECONDS`, then holds `TO`. `--stall-every` and `--stall-ms` add to either a constant delay or a ramp. Video packets leave the relay in the order they came, so a ramp down bunches them up rather than reordering them.

The relay finds the video PID from the PAT and the PMT (stream types H.264 and HEVC). Pass `--video-pid` (decimal or `0x` hex) for a stream that declares its video otherwise. Every 5 s it prints the current delay, how many video packets it holds, and the packets in and out per PID.

## Read the stats line

The plugin logs a stats line every 30 seconds to the OBS log (Help, Log Files). The three fields that matter here:

```shell
grep -o "av_skew=[^ ]* hold=[^ ]* vdelay=[^ ]*" <obs log file>
```

- `av_skew` is the video PTS minus the audio PTS of what last reached the plugin. With the relay it reads about minus the relay's delay, plus the baseline. A stall makes it dip further while it lasts.
- `hold` is how much longer than Target Buffer the audio waits. It covers the part of the skew that Target Buffer and the output lead do not, plus a 100 ms margin. A sender that is late from the first packet is covered before audio starts. A skew that appears later raises the hold once it lasts 2 s, and the hold falls again after 10 s of video that needed less. In Low Latency Audio mode, or with Adaptive Latency Control off, the hold is sized only when the stream starts.
- `vdelay` is the standing delay on the video schedule. It bridges a skew until the hold is built, then returns to zero. A `vdelay` that stays above zero while the hold stands means video is late for a reason on this machine, such as decoding.

## Measure lip sync from a recording

1. Set the OBS recording format to MKV.
2. Wait until the stats line shows the hold settled, then record for 30 s.
3. List the flash frames:

   ```shell
   ffmpeg -hide_banner -i recording.mkv -an \
     -vf "signalstats,metadata=mode=select:key=lavfi.signalstats.YAVG:value=200:function=greater,metadata=mode=print" \
     -f null - 2>&1 | grep -o "pts_time:[0-9.]*"
   ```

   Each flash shows up as one to three consecutive frames. The first of each run is the flash.

4. List the beep onsets:

   ```shell
   ffmpeg -hide_banner -i recording.mkv -vn -af "silencedetect=noise=-20dB:duration=0.5" \
     -f null - 2>&1 | grep -o "silence_end: [0-9.]*"
   ```

   The quiet bed stays under the -20 dB threshold, so the gaps between beeps count as silence and each `silence_end` is a beep onset. Ignore the last one, which is the end of the file.

5. For each second, subtract the beep onset from the flash time. A positive result is the picture behind the sound. Subtract the baseline.

The rig resolves one video frame (33 ms at 30 fps) and one AAC frame (21 ms). Treat differences below that as noise. The same check works by eye in a video editor: step to the first white frame and compare it with the start of the beep in the waveform.

## The automated counterpart

`crates/irl-source/tests/av_sync_sim.rs` runs the same cases (audio ahead with recurring video stalls, video seconds behind its audio, a skew that rises and recovers) through the receiver's intake, the audio pump and the video thread on a virtual clock, and measures the offset libobs is handed. Use the relay to confirm a sim result on a real OBS, and turn anything the relay finds into a case in the sim.
