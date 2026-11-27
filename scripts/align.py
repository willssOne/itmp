#!/usr/bin/env python3
"""Forced-aligns known LRC lyrics against a song and writes per-word timestamps.

usage: align.py <audio> <lyrics.lrc> <out.json> [model]

Whisper is not asked to *guess* the lyrics: the LRC text is aligned to the audio,
so only the timing comes from the model. Lines it cannot place confidently are
written as null and the player falls back to its estimate for those.
"""
import difflib
import json
import os
import re
import statistics
import subprocess
import sys

SR = 16000
CHUNK = 29.0
PAD = 0.6
STAMP = re.compile(r"\[(\d+):(\d+(?:\.\d+)?)\]")


def read_lrc(path):
    events = []
    with open(path, encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            rest = raw.strip()
            times = []
            while True:
                m = STAMP.match(rest)
                if not m:
                    break
                times.append(int(m[1]) * 60 + float(m[2]))
                rest = rest[m.end():].lstrip()
            text = re.sub(r"<\d+:\d+(?:\.\d+)?>", "", rest).strip()
            events.extend((t, text) for t in times)
    events.sort(key=lambda e: e[0])
    lines = []
    for i, (t, text) in enumerate(events):
        if text:
            end = events[i + 1][0] if i + 1 < len(events) else t + 6.0
            lines.append({"t": t, "end": end, "text": text})
    return lines


def decode(path):
    # ffmpeg is already required by itmp and is steadier than PyAV across versions.
    import numpy as np

    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-nostdin", "-i", path, "-ac", "1", "-ar", str(SR), "-f", "f32le", "-"],
        check=True, stdout=subprocess.PIPE,
    ).stdout
    return np.frombuffer(raw, dtype=np.float32)


def norm(word):
    return "".join(c for c in word.lower() if c.isalnum())


def chunks(lines, offset, duration):
    group = []
    for line in lines:
        if group:
            start = group[0]["t"] + offset - PAD
            end = min(line["end"], line["t"] + 8.0) + offset + PAD
            if end - start > CHUNK:
                yield group
                group = []
        group.append(line)
    if group:
        yield group


def align_pass(model, tokenizer, audio, lines, offset):
    from faster_whisper.audio import pad_or_trim

    duration = len(audio) / SR
    results = {}
    for group in chunks(lines, offset, duration):
        start = max(0.0, group[0]["t"] + offset - PAD)
        last = group[-1]
        end = min(duration, min(last["end"], last["t"] + 8.0) + offset + PAD, start + CHUNK)
        if end - start < 0.5:
            continue
        clip = audio[int(start * SR):int(end * SR)]
        features = model.feature_extractor(clip)
        num_frames = min(features.shape[-1], model.feature_extractor.nb_max_frames)
        encoded = model.encode(pad_or_trim(features))
        words = [w for line in group for w in line["text"].split()]
        tokens = tokenizer.encode(" " + " ".join(words))[:440]
        pieces = model.find_alignment(tokenizer, [tokens], encoded, num_frames)[0]

        # Map the tokenizer's pieces back onto whitespace words via character offsets.
        spans = []
        pos = 0
        for p in pieces:
            n = len(norm(p["word"]))
            if n:
                spans.append((pos, pos + n, float(p["start"]), float(p["end"]), float(p["probability"])))
            pos += n
        pos = 0
        timed = []
        for w in words:
            n = len(norm(w))
            hit = [s for s in spans if s[0] < pos + n and s[1] > pos] if n else []
            if hit:
                timed.append((start + hit[0][2], start + hit[-1][3], sum(h[4] for h in hit) / len(hit)))
            else:
                timed.append(None)
            pos += n
        i = 0
        for line in group:
            count = len(line["text"].split())
            results[line["t"]] = timed[i:i + count]
            i += count
    return results


def spread(words, limit):
    """Evens out runs of words the model stacked onto the same instant."""
    i = 0
    while i < len(words):
        j = i
        while j + 1 < len(words) and words[j + 1][0] - words[j][0] < 0.07:
            j += 1
        if j - i >= 2:
            start = words[i][0]
            stop = words[j + 1][0] if j + 1 < len(words) else limit
            stop = min(max(stop, words[j][1]), start + 0.28 * (j - i + 1))
            step = (stop - start) / (j - i + 1)
            if step > 0.02:
                for k in range(i, j + 1):
                    words[k] = [round(start + step * (k - i), 3), round(start + step * (k - i + 1), 3)]
        i = j + 1


def judge(lines, results, offset):
    """Returns {line_time: [[start, end], ...]} for the lines that look right."""
    good, probs = {}, []
    previous = 0.0
    for line in lines:
        timed = results.get(line["t"])
        if not timed:
            continue
        known = [t for t in timed if t]
        if not known:
            continue
        prob = sum(k[2] for k in known) / len(known)
        probs.append(prob)
        first, last = known[0][0], known[-1][1]
        expected = line["t"] + offset
        if prob < 0.08 or abs(first - expected) > 1.8 or last > line["end"] + offset + 1.5:
            continue
        if last - first < 0.04 * len(known) or first < previous - 0.35:
            continue
        words, cursor = [], first
        for t in timed:
            s, e = (t[0], t[1]) if t else (cursor, cursor)
            s = max(s, cursor)
            e = max(e, s + 0.06)
            words.append([round(s, 3), round(e, 3)])
            cursor = s + 0.02
        spread(words, min(line["end"] + offset, words[-1][1] + 0.9))
        previous = words[-1][0]
        good[line["t"]] = words
    score = (len(good) / len(lines)) * (statistics.mean(probs) if probs else 0.0)
    return good, score


def estimate_offset(model, audio, lines):
    """Finds a whole-song shift (different intro/master) from a free transcription."""
    segments, _ = model.transcribe(
        audio, language="en", beam_size=1, word_timestamps=True,
        condition_on_previous_text=False, vad_filter=False,
    )
    heard = [(norm(w.word), w.start) for s in segments for w in (s.words or []) if norm(w.word)]
    wanted = []
    for line in lines:
        for i, w in enumerate(line["text"].split()):
            if norm(w):
                wanted.append((norm(w), line["t"] if i == 0 else None))
    matcher = difflib.SequenceMatcher(None, [w for w, _ in wanted], [w for w, _ in heard], autojunk=False)
    deltas = []
    for a, b, size in matcher.get_matching_blocks():
        if size >= 3 and wanted[a][1] is not None:
            deltas.append(heard[b][1] - wanted[a][1])
    if len(deltas) < 3:
        return None
    mid = statistics.median(deltas)
    close = [d for d in deltas if abs(d - mid) < 1.0]
    return statistics.median(close) if len(close) >= max(3, len(deltas) // 2) else None


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    audio_path, lrc_path, out_path = sys.argv[1:4]
    name = sys.argv[4] if len(sys.argv) > 4 else "small.en"
    lines = read_lrc(lrc_path)
    if not lines:
        sys.exit("no timed lyrics")

    from faster_whisper import WhisperModel
    from faster_whisper.tokenizer import Tokenizer

    threads = max(2, (os.cpu_count() or 4) // 2)
    model = WhisperModel(name, device="cpu", compute_type="int8", cpu_threads=threads)
    tokenizer = Tokenizer(model.hf_tokenizer, model.model.is_multilingual, task="transcribe", language="en")
    audio = decode(audio_path)

    offset = 0.0
    good, score = judge(lines, align_pass(model, tokenizer, audio, lines, offset), offset)
    if len(good) < 0.7 * len(lines):
        shift = estimate_offset(model, audio, lines)
        if shift is not None and abs(shift) > 0.7:
            other, other_score = judge(lines, align_pass(model, tokenizer, audio, lines, shift), shift)
            if other_score > score:
                good, offset = other, shift

    # Too few confident lines means these lyrics do not belong to this audio.
    trusted = len(good) >= max(3, int(0.35 * len(lines)))
    payload = {
        "version": 1,
        "offset": round(offset, 3) if trusted else 0.0,
        "matched": len(good) if trusted else 0,
        "total": len(lines),
        "lines": [{"t": t, "words": w} for t, w in sorted(good.items())] if trusted else [],
    }
    tmp = out_path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(payload, fh)
    os.replace(tmp, out_path)
    print(f"aligned {payload['matched']}/{payload['total']} lines, offset {payload['offset']:+.2f}s")


if __name__ == "__main__":
    main()
