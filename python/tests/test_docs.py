"""Checks the documentation: docstrings, signatures, the type stub, and the examples in the docs."""

import ast
import importlib.util
import inspect
import pydoc
import re
import sys
from pathlib import Path

import pytest

import lazy_zip
import lazy_zip.__main__ as cli
from lazy_zip import _lazy_zip

ROOT = Path(__file__).resolve().parent.parent
README = ROOT / "README.md"
GUIDE = ROOT / "docs" / "integration.md"
DOCS = [README, GUIDE]
STUB = Path(lazy_zip.__file__).with_name("_lazy_zip.pyi")

# The examples use this URL. The tests serve conftest.EXAMPLE as files.zip in its place.
EXAMPLE_BASE = "https://example.com/"

REQUIRED = inspect.Parameter.empty
# Dunder methods that are part of the documented API, rather than Python's object protocol.
DOCUMENTED_DUNDERS = {"__enter__", "__exit__"}


def public_classes():
    classes = {name: getattr(lazy_zip, name) for name in lazy_zip.__all__}
    classes["EntryStream"] = _lazy_zip.EntryStream
    classes["_lazy_zip.RemoteZip"] = _lazy_zip.RemoteZip
    return classes


def public_members(cls):
    """(name, object) for the attributes `cls` defines itself, skipping private ones."""
    for name, member in vars(cls).items():
        if name.startswith("_") and name not in DOCUMENTED_DUNDERS:
            continue
        yield name, member


def documented_items():
    items = [
        ("lazy_zip", lazy_zip),
        ("lazy_zip._lazy_zip", _lazy_zip),
        ("lazy_zip.__main__", cli),
        ("lazy_zip.__main__.main", cli.main),
    ]
    for class_name, cls in public_classes().items():
        items.append((class_name, cls))
        for name, member in public_members(cls):
            items.append((f"{class_name}.{name}", member))
    return items


# Docstrings


@pytest.mark.parametrize("name, item", documented_items(), ids=[n for n, _ in documented_items()])
def test_everything_public_has_a_docstring(name, item):
    doc = item.__doc__ if not isinstance(item, (staticmethod, classmethod)) else item.__func__.__doc__
    assert doc and doc.strip(), f"{name} has no docstring"


def test_exception_docstrings_describe_their_attributes():
    assert "kind" in lazy_zip.BadZipFile.__doc__
    for kind in ("not_a_zip", "limit_exceeded", "corrupt", "suspicious", "unsafe"):
        assert kind in lazy_zip.BadZipFile.__doc__
    assert "failures" in lazy_zip.ExtractError.__doc__


def test_module_docstring_lists_every_kind():
    kinds = re.findall(r'^\| `"(\w+)"` \|', README.read_text(encoding="utf-8"), re.M)
    assert len(kinds) == 14
    for kind in kinds:
        assert f'"{kind}"' in lazy_zip.__doc__


# Signatures


def params(*names, **defaults):
    return [(name, REQUIRED) for name in names] + list(defaults.items())


SIGNATURES = [
    (lazy_zip.RemoteZip, params("url", headers=None, timeout=None)),
    (lazy_zip.RemoteZip.open, params("name", mode="r")),
    (lazy_zip.RemoteZip.printdir, params(file=None)),
    (lazy_zip.RemoteZip.namelist, params()),
    (lazy_zip.RemoteZip.infolist, params()),
    (lazy_zip.RemoteZip.getinfo, params("name")),
    (lazy_zip.RemoteZip.read, params("name")),
    (lazy_zip.RemoteZip.extract, params("member", path=None)),
    (lazy_zip.RemoteZip.extractall, params(path=None, members=None, workers=8)),
    (lazy_zip.RemoteZip.testzip, params()),
    (lazy_zip.RemoteZip.read_to_file, params("path", "filepath")),
    (lazy_zip.RemoteZip.load, params()),
    (lazy_zip.RemoteZip.close, params()),
    (lazy_zip.RemoteZip.__enter__, params()),
    (lazy_zip.RemoteZip.__exit__, params("exc_type", "exc_value", "traceback")),
    (lazy_zip.ZipInfo.is_dir, params()),
    (_lazy_zip.EntryStream.read, params(size=-1)),
    (_lazy_zip.EntryStream.seek, params("offset", whence=0)),
    (_lazy_zip.EntryStream.tell, params()),
    (_lazy_zip.EntryStream.close, params()),
    (cli.main, params(argv=None)),
]


@pytest.mark.parametrize("function, expected", SIGNATURES, ids=[f.__qualname__ for f, _ in SIGNATURES])
def test_signatures_are_introspectable(function, expected):
    signature = inspect.signature(function)
    actual = [(p.name, p.default) for p in signature.parameters.values() if p.name != "self"]
    assert actual == expected


def test_help_shows_signatures_and_docs():
    text = pydoc.render_doc(lazy_zip.RemoteZip, renderer=pydoc.plaintext)
    assert "extractall(self, /, path=None, members=None, workers=8)" in text
    assert "open(self, name: Union[str, lazy_zip.ZipInfo], mode: str = 'r')" in text
    assert "network_bytes" in text and "Bytes received from the server" in text
    assert "Ellipsis" not in text


# The stub


def stub_tree():
    return ast.parse(STUB.read_text(encoding="utf-8"))


def stub_classes():
    classes = {}
    for node in stub_tree().body:
        if isinstance(node, ast.ClassDef):
            classes[node.name] = node
    return classes


def is_property(function):
    return any(isinstance(d, ast.Name) and d.id == "property" for d in function.decorator_list)


def runtime_public_names(cls):
    return {name for name, _ in public_members(cls)}


def stub_members(node):
    functions = {item.name: item for item in node.body if isinstance(item, ast.FunctionDef)}
    attributes = {item.target.id for item in node.body if isinstance(item, ast.AnnAssign)}
    return functions, attributes


def test_stub_lists_the_runtime_names():
    runtime = {name for name in vars(_lazy_zip) if not name.startswith("_")}
    assert set(stub_classes()) == runtime


@pytest.mark.parametrize("class_name", sorted(stub_classes()))
def test_stub_members_match_the_runtime(class_name):
    cls = getattr(_lazy_zip, class_name)
    functions, _ = stub_members(stub_classes()[class_name])
    stubbed = {n for n in functions if not n.startswith("__") or n in DOCUMENTED_DUNDERS}
    # Private names the stub declares on purpose, for the Python wrapper.
    runtime = runtime_public_names(cls) | ({"_open_stream"} & set(vars(cls)))
    assert stubbed == runtime

    for name in stubbed:
        member = vars(cls)[name]
        function = functions[name]
        if is_property(function):
            assert isinstance(member, (property, type(_lazy_zip.ZipInfo.filename))), name
            continue
        assert callable(member), name
        args = function.args
        stub_names = [a.arg for a in args.posonlyargs + args.args if a.arg != "self"]
        defaults = [ast.literal_eval(d) for d in args.defaults]
        stub_params = list(zip(stub_names, [REQUIRED] * (len(stub_names) - len(defaults)) + defaults))
        runtime_params = [
            (p.name, p.default) for p in inspect.signature(member).parameters.values() if p.name != "self"
        ]
        assert stub_params == runtime_params, name


def test_stub_constructor_matches_the_runtime():
    new = next(f for f in stub_classes()["RemoteZip"].body if getattr(f, "name", None) == "__new__")
    names = [a.arg for a in new.args.args if a.arg != "cls"]
    defaults = [ast.literal_eval(d) for d in new.args.defaults]
    runtime = inspect.signature(_lazy_zip.RemoteZip).parameters.values()
    assert names == [p.name for p in runtime]
    assert defaults == [p.default for p in runtime if p.default is not REQUIRED]


def test_stub_has_docstrings():
    assert ast.get_docstring(stub_tree())
    for name, node in stub_classes().items():
        assert ast.get_docstring(node), name
        functions, _ = stub_members(node)
        for function in functions.values():
            if function.name != "__new__":
                assert ast.get_docstring(function), f"{name}.{function.name}"


def test_stub_instance_attributes_exist(server, tmp_path):
    _, bad_zip_attributes = stub_members(stub_classes()["BadZipFile"])
    _, extract_error_attributes = stub_members(stub_classes()["ExtractError"])
    assert bad_zip_attributes == {"kind"} and extract_error_attributes == {"failures"}
    with lazy_zip.RemoteZip(server.url("/unsafe.zip")) as archive:
        with pytest.raises(lazy_zip.ExtractError) as raised:
            archive.extractall(tmp_path)
    assert isinstance(raised.value.failures, dict)
    failure = raised.value.failures["../evil.txt"]
    assert isinstance(failure, lazy_zip.BadZipFile) and failure.kind == "unsafe"


# Examples in the docs

BLOCK = re.compile(
    r"(?:<!-- test: (?P<marker>[^>]*?) -->\n)?^```(?P<lang>\w*)\n(?P<code>.*?)^```$", re.S | re.M
)


def code_blocks(path, lang="python"):
    """(line, marker, code) for each fenced block in `lang`."""
    text = path.read_text(encoding="utf-8")
    blocks = []
    for match in BLOCK.finditer(text):
        if match.group("lang") == lang:
            line = text.count("\n", 0, match.start("code")) + 1
            blocks.append((line, match.group("marker"), match.group("code")))
    return blocks


ALL_BLOCKS = [(path, *block) for path in DOCS for block in code_blocks(path)]


def test_docs_have_examples():
    assert len(code_blocks(README)) >= 3
    assert len(code_blocks(GUIDE)) >= 20


@pytest.mark.parametrize(
    "path, line, marker, code", ALL_BLOCKS, ids=[f"{p.name}:{line}" for p, line, _, _ in ALL_BLOCKS]
)
def test_examples_compile(path, line, marker, code):
    compile(code, f"{path.name}:{line}", "exec")


@pytest.mark.skipif(sys.version_info < (3, 9), reason="the examples use asyncio.to_thread")
@pytest.mark.parametrize("path", DOCS, ids=[p.name for p in DOCS])
def test_examples_run(path, server, tmp_path, monkeypatch):
    """Runs a document's examples in order, in one namespace, like a reader following along."""
    monkeypatch.chdir(tmp_path)
    namespace = {"__name__": "docs_example"}
    ran = 0
    for line, marker, code in code_blocks(path):
        if marker == "compile-only":
            continue
        if marker and marker.startswith("requires "):
            if importlib.util.find_spec(marker.split()[1]) is None:
                continue
        elif marker:
            pytest.fail(f"{path.name}:{line}: unknown marker {marker!r}")
        code = code.replace(EXAMPLE_BASE, server.url("/"))
        exec(compile(code, f"{path.name}:{line}", "exec"), namespace)
        ran += 1
    assert ran >= 3


def slug(heading):
    """The anchor GitHub gives a heading."""
    return re.sub(r"[^\w\- ]", "", heading.strip().lower()).replace(" ", "-")


def anchors(path):
    text = re.sub(r"^```.*?^```$", "", path.read_text(encoding="utf-8"), flags=re.S | re.M)
    return {slug(heading) for heading in re.findall(r"^#+ (.+)$", text, re.M)}


@pytest.mark.parametrize("path", DOCS, ids=[p.name for p in DOCS])
def test_links_between_docs_resolve(path):
    for target, anchor in re.findall(r"\]\(([^)#\s]*)(?:#([^)\s]+))?\)", path.read_text(encoding="utf-8")):
        if target.startswith("http"):
            continue
        linked = (path.parent / target).resolve() if target else path
        assert linked.exists(), f"{path.name} links to missing {target}"
        if anchor:
            assert anchor in anchors(linked), f"{path.name} links to missing #{anchor} in {linked.name}"


def test_readme_documents_every_cli_option(capsys):
    with pytest.raises(SystemExit):
        cli.main(["--help"])
    options = set(re.findall(r"(?<![\w-])--[a-z]+", capsys.readouterr().out)) - {"--help"}
    assert options == {"--list", "--dir", "--header", "--workers", "--timeout"}
    readme = README.read_text(encoding="utf-8")
    for option in options:
        assert f"`{option}" in readme or f", `{option}" in readme, option
