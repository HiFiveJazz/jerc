#!/usr/bin/env python3
"""Verify a generated Gentoo Manifest against its GitHub binary source."""
import argparse
import hashlib
from pathlib import Path
import tarfile


def verify(tag: str, directory: Path) -> None:
    version = tag.removeprefix('v')
    binary = directory / f'jerc-{tag}-x86_64-unknown-linux-musl.tar.gz'
    overlay = directory / f'jerc-gentoo-overlay-{tag}.tar.gz'
    with tarfile.open(overlay, 'r:gz') as tf:
        root = 'jerc-overlay/app-misc/jerc-bin/'
        ebuild_name = f'{root}jerc-bin-{version}.ebuild'
        ebuild = tf.extractfile(ebuild_name).read().decode()
        manifest = tf.extractfile(root+'Manifest').read().decode().strip().split()
        assert f'jerc-v${{PV}}-x86_64-unknown-linux-musl.tar.gz' in ebuild
        assert manifest[:3] == ['DIST', binary.name, str(binary.stat().st_size)]
        assert manifest[3] == 'BLAKE2B' and manifest[5] == 'SHA512'
        b2 = hashlib.blake2b()
        sha = hashlib.sha512()
        with binary.open('rb') as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b''):
                b2.update(chunk)
                sha.update(chunk)
        assert manifest[4] == b2.hexdigest() and manifest[6] == sha.hexdigest()
        assert tf.extractfile('jerc-overlay/profiles/repo_name').read().strip() == b'jerc-local'
    print(f'Verified Gentoo overlay for {tag}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('tag')
    parser.add_argument('--dist', type=Path, default=Path('dist'))
    args = parser.parse_args()
    verify(args.tag, args.dist)
