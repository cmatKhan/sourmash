"""Define the top-level command line interface for sourmash

This module handles user input when sourmash is invoked from the command line.
A top-level parser is defined for the `sourmash` command, and subparsers are
defined for each subcommand. Some sourmash operations are grouped together
using the `sourmash <subcmd> <subsubcmd>` pattern, and these are organized in
their own CLI submodules, each with a dedicated directory.
"""

from argparse import ArgumentParser, RawDescriptionHelpFormatter, SUPPRESS
import os
import sys

import sourmash

from . import utils

# Commands
from . import categorize
from . import compare
from . import compute
from . import gather
from . import import_csv
from . import info
from . import index
from . import migrate
from . import multigather
from . import plot
from . import prefetch
from . import sbt_combine
from . import search
from . import watch

# Subcommand groups
from . import lca
from . import sig
from . import sig as signature
from . import sketch
from . import storage
from . import tax
from . import scripts


class SourmashParser(ArgumentParser):
    _citation_printed = False

    def __init__(self, citation=True, **kwargs):
        super().__init__(**kwargs)
        self.citation = citation

    @classmethod
    def print_citation(cls):
        if cls._citation_printed:
            return
        from sourmash.logging import notify

        notify(f"\n== This is sourmash version {sourmash.VERSION}. ==")
        notify("== Please cite Irber et. al (2024), doi:10.21105/joss.06830. ==\n")
        cls._citation_printed = True

    def _subparser_from_name(self, name):
        """Given a name, get the subparser instance registered with this parser."""
        container = self._actions
        if name is None:
            return None
        for action in container:
            if action.choices is None:
                continue
            elif name in action.choices:
                return action.choices[name]

    def print_help(self):
        self.print_citation()
        super().print_help()

    def parse_args(self, args=None, namespace=None):
        if (args is None and len(sys.argv) == 1) or (
            args is not None and len(args) == 0
        ):
            self.print_help()
            raise SystemExit(1)
        args = super().parse_args(args=args, namespace=namespace)
        if ("quiet" not in args or not args.quiet) and self.citation:
            self.print_citation()

        if "subcmd" in args and args.subcmd is None:
            self._subparser_from_name(args.cmd).print_help()
            raise SystemExit(1)

        # BEGIN: dirty hacks to simultaneously support new and previous interface
        if hasattr(args, "subcmd") and args.subcmd == "import":
            args.subcmd = "ingest"
        # END: dirty hacks to simultaneously support new and previous interface
        return args


def get_parser():
    module_descs = {
        "tax": 'Integrate taxonomy information based on "gather" results',
        "lca": "Taxonomic operations",
        "sketch": "Create signatures",
        "sig": "Manipulate signature files",
        "storage": "Operations on storage",
        "scripts": "Plug-ins",
    }
    alias = {
        "sig": "signature",
        "ext": "scripts",
    }
    expert = set(
        ["categorize", "import_csv", "migrate", "multigather", "sbt_combine", "watch"]
    )

    clidir = os.path.dirname(__file__)
    basic_ops = utils.command_list(clidir)

    # provide a list of the basic operations - not expert, not submodules.
    user_ops = [op for op in basic_ops if op not in expert and op not in module_descs]
    usage = "    Basic operations\n"
    for op in user_ops:
        docstring = getattr(sys.modules[__name__], op).__doc__
        helpstring = f"sourmash {op:s} --help"
        usage += f"        {helpstring:25s} {docstring:s}\n"
    # next, all the subcommand ones - dive into subdirectories.
    cmd_group_dirs = next(os.walk(clidir))[1]
    cmd_group_dirs = filter(utils.opfilter, cmd_group_dirs)
    cmd_group_dirs = sorted(cmd_group_dirs)

    cmd_group_usage = [cmd for cmd in cmd_group_dirs if cmd not in alias.values()]
    for dirpath in cmd_group_usage:
        usage += "\n    " + module_descs[dirpath] + "\n"
        usage += f"        sourmash {dirpath:s} --help\n"
        if dirpath in alias:
            usage += f"        sourmash {alias[dirpath]:s} --help\n"

    desc = (
        "Create, compare, and manipulate k-mer sketches of biological sequences.\n\nUsage instructions:\n"
        + usage
    )
    parser = SourmashParser(
        prog="sourmash",
        description=desc,
        formatter_class=RawDescriptionHelpFormatter,
        usage=SUPPRESS,
    )
    parser._optionals.title = "Options"
    parser.add_argument(
        "-v", "--version", action="version", version="sourmash " + sourmash.VERSION
    )
    parser.add_argument(
        "-q", "--quiet", action="store_true", help="don't print citation information"
    )
    sub = parser.add_subparsers(
        title="Instructions",
        dest="cmd",
        metavar="cmd",
        help=SUPPRESS,
    )
    for op in basic_ops + cmd_group_dirs:
        getattr(sys.modules[__name__], op).subparser(sub)
    parser._action_groups.reverse()
    return parser


def parse_args(arglist=None):
    """
    Return an argparse 'args' object from parsing arglist.

    By default pulls arguments from sys.argv.

    Example usage:

    ```
    args = parse_args(['sig', 'filter', '-m', '10'])

    sourmash.sig.filter.__main__.filter(args)
    ```
    """
    return get_parser().parse_args(arglist)
