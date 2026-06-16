import tempfile
import xg


def test_open_returns_xgrep():
    with tempfile.TemporaryDirectory() as d:
        engine = xg.Xgrep.open(d)
        assert engine.root == d


def test_open_local_returns_xgrep():
    with tempfile.TemporaryDirectory() as d:
        engine = xg.Xgrep.open_local(d)
        assert engine.root == d


def test_build_and_search(tmp_path):
    (tmp_path / "hello.py").write_text("def greet():\n    return 'hello'\n")
    engine = xg.Xgrep.open(str(tmp_path))
    engine.build_index()
    results = engine.search("greet")
    assert len(results) == 1  # "greet" は def greet(): の1行のみ
    assert isinstance(results[0], xg.SearchResult)
    assert results[0].file.endswith("hello.py")
    assert results[0].line_number >= 1
    assert "greet" in results[0].line


def test_search_with_file_type(tmp_path):
    (tmp_path / "code.rs").write_text("fn greet() {}\n")
    (tmp_path / "code.py").write_text("def greet(): pass\n")
    engine = xg.Xgrep.open(str(tmp_path))
    engine.build_index()
    results = engine.search("greet", file_type="rs")
    assert len(results) == 1
    assert results[0].file.endswith(".rs")


def test_search_case_insensitive(tmp_path):
    (tmp_path / "file.txt").write_text("Hello World\n")
    engine = xg.Xgrep.open(str(tmp_path))
    engine.build_index()
    results = engine.search("hello", case_insensitive=True)
    assert len(results) == 1


def test_search_max_count(tmp_path):
    content = "match\n" * 10
    (tmp_path / "file.txt").write_text(content)
    engine = xg.Xgrep.open(str(tmp_path))
    engine.build_index()
    results = engine.search("match", max_count=3)
    assert len(results) <= 3


def test_index_status(tmp_path):
    engine = xg.Xgrep.open(str(tmp_path))
    status = engine.index_status()
    assert status.state in ("fresh", "stale", "missing")
    assert isinstance(status.indexed_files, int)
    assert isinstance(status.index_size_bytes, int)
    assert isinstance(status.index_path, str)


def test_search_result_fields(tmp_path):
    (tmp_path / "sample.rs").write_text("fn main() {}\n")
    engine = xg.Xgrep.open(str(tmp_path))
    engine.build_index()
    results = engine.search("main")
    assert len(results) >= 1
    r = results[0]
    assert isinstance(r.file, str)
    assert isinstance(r.line_number, int)
    assert isinstance(r.line, str)
    assert r.line_number >= 1


def test_index_path_property(tmp_path):
    engine = xg.Xgrep.open(str(tmp_path))
    assert isinstance(engine.index_path, str)
    assert len(engine.index_path) > 0
