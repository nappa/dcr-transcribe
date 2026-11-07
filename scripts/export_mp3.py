#!/usr/bin/env python3
"""Export finalized, same-channel WAV parts in argument order to an MP3 review bundle."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import wave


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def export(inputs, destination):
    if not inputs:
        raise ValueError('WAV ファイルを指定してください')
    if destination.exists():
        raise ValueError('出力先が既に存在します（上書きしません）')
    encoder = shutil.which('ffmpeg')
    if not encoder:
        raise ValueError('ffmpeg (libmp3lame 対応) が必要です')
    records, signature, offset = [], None, 0
    for path in inputs:
        path = path.resolve(strict=True)
        timing = path.with_suffix('.timing.jsonl')
        events = [json.loads(line) for line in timing.read_text().splitlines()]
        if not events or events[0].get('type') != 'header' or events[-1].get('type') != 'finalized':
            raise ValueError(f'正常終了した時刻記録がありません: {path}')
        with wave.open(str(path), 'rb') as audio:
            rate, frames = audio.getframerate(), audio.getnframes()
            if audio.getnchannels() != 1 or audio.getsampwidth() != 2:
                raise ValueError(f'16-bit mono WAV が必要です: {path}')
        header = events[0]
        current = (header['channel'], rate)
        if header['sample_rate'] != rate or events[-1]['samples'] != frames:
            raise ValueError(f'音声と時刻記録が一致しません: {path}')
        if signature is not None and signature != current:
            raise ValueError('同一チャンネル・同一サンプルレートの音声を指定してください')
        signature = current
        records.append(dict(source=str(path), sha256=sha256(path),
                            timing_sha256=sha256(timing), timing=events,
                            mp3_pcm_start_sample=offset, samples=frames,
                            stat=(path.stat().st_size, path.stat().st_mtime_ns)))
        offset += frames
    destination.parent.mkdir(parents=True, exist_ok=True)
    # 公開する前に同じファイルシステム上で一式を完成させる。
    with tempfile.TemporaryDirectory(prefix='.mp3-export-', dir=destination.parent) as work:
        work = Path(work)
        lines = []
        for i, record in enumerate(records):
            name = f'part{i:06}.wav'
            (work / name).symlink_to(record['source'])
            lines.append(f"file '{name}'\n")
        (work / 'input.txt').write_text(''.join(lines))
        bundle = work / 'bundle'
        bundle.mkdir()
        subprocess.run([encoder, '-nostdin', '-v', 'error', '-n', '-f', 'concat',
                        '-safe', '0', '-i', str(work / 'input.txt'), '-map_metadata', '-1',
                        '-c:a', 'libmp3lame', '-b:a', '64k', str(bundle / 'review.mp3')], check=True)
        for record in records:
            path = Path(record['source'])
            if record.pop('stat') != (path.stat().st_size, path.stat().st_mtime_ns):
                raise ValueError(f'書き出し中に録音が変更されました: {path}')
        manifest = dict(version=1, sample_rate=signature[1], channel=signature[0],
                        clock='pc_system_time_unverified', gap_policy='omitted_audio_not_padded',
                        mp3_sha256=sha256(bundle / 'review.mp3'), sources=records)
        (bundle / 'timeline.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + '\n')
        # mkdir により競合時も既存の出力を上書きしない。
        destination.mkdir()
        try:
            for path in bundle.iterdir():
                os.link(path, destination / path.name)
        except Exception:
            # この実行で作ったファイルのみ片付ける。
            for name in ('review.mp3', 'timeline.json'):
                (destination / name).unlink(missing_ok=True)
            destination.rmdir()
            raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('wav', type=Path, nargs='+', help='同一チャンネルの WAV を再生順に指定')
    parser.add_argument('--output', type=Path, required=True, help='新規の出力ディレクトリ')
    args = parser.parse_args()
    try:
        export(args.wav, args.output)
    except (OSError, ValueError, KeyError, wave.Error, subprocess.CalledProcessError) as error:
        parser.exit(1, f'書き出し失敗: {error}\n')
    print(args.output / 'review.mp3')


if __name__ == '__main__':
    main()
