"""PEP 616 / bpo-39939: literal prefix, not a character set."""


def remove_prefix(s, prefix):
    if s.startswith(prefix):
        return s[len(prefix) :]
    return s[:]
