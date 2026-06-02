"""Minimal setup.py: tag the wheel as platform-specific (platlib).

The package bundles a `libcamera_protocol_ffi.{so,dylib}` next to the
generated Python module, so a wheel built on linux/x86_64 is NOT compatible
with macOS/arm64. setuptools' default for pure-Python projects is to mark
the wheel as universal — that produces a "py3-none-any" tag and downstream
installers won't reject it on mismatched platforms. Overriding
`has_ext_modules` flips the wheel to the right platform tag (e.g.
`cp39-abi3-linux_x86_64`).
"""

from setuptools import setup
from setuptools.dist import Distribution


class BinaryDistribution(Distribution):
    """Tag the wheel as platform-specific because it ships a native lib."""

    def has_ext_modules(self) -> bool:  # noqa: D401
        return True


setup(distclass=BinaryDistribution)
