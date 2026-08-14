"""Wrong PEP 616 workaround: lstrip treats the argument as a character set."""


def remove_prefix(s, prefix):
    return s.lstrip(prefix)
