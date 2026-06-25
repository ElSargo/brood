#!/usr/bin/env python3
import argparse
import sys

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--title", required=True)
    parser.add_argument("--label", required=True)
    args = parser.parse_args()

    window = Gtk.Window(title=args.title)
    window.set_default_size(520, 260)
    window.connect("destroy", Gtk.main_quit)

    label = Gtk.Label(label=args.label)
    label.set_margin_top(32)
    label.set_margin_bottom(32)
    label.set_margin_start(32)
    label.set_margin_end(32)
    window.add(label)
    window.show_all()
    Gtk.main()


if __name__ == "__main__":
    try:
        main()
    except Exception as err:
        print(f"ERR {err}", file=sys.stderr)
        raise
