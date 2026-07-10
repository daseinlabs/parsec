import os
import pathlib

import pytest

CKPT = pathlib.Path(os.path.expanduser("~/.dasein/brain/curator_v4_prod.pt"))


@pytest.fixture(scope="session")
def base_ckpt() -> pathlib.Path:
    if not CKPT.is_file():
        pytest.skip(
            f"checkpoint missing: {CKPT} — "
            "gsutil cp gs://dasein-473321-ac-learning/rulehead/curator_v4_prod.pt ~/.dasein/brain/"
        )
    return CKPT
