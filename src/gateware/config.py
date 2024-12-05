import os
from artiq._version import get_version


def generate_ident(variant):
    return "{}+{};{}".format(
        get_version().split(".")[0],
        os.getenv("ZYNQ_REV", default="unknown")[:8],
        variant,
    )
