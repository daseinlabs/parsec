"""Concrete arms. Importing this package registers every arm.

The registry (and the built-in `baseline` control) lives in parsec_bench.arm;
one module per arm here, self-registering via @register on import.
"""

from parsec_bench.arms import parsec  # noqa: F401  (side effect: registers "parsec")
