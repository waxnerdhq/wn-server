"""wn_candidates: writes one JSON line per album import task.

wn-server reads the file named by the ``output`` option after
``beet import -q`` exits. Each line is an object with these keys:

- ``folder``: absolute path of the album directory. For an album that beets
  collapsed from several directories (for example CD1 and CD2), it is their
  common parent.
- ``tags``: the existing tags, ``artist``, ``album``, ``year``, and
  ``track_count``.
- ``decision``: ``applied``, ``skipped``, ``duplicate``, or ``error``.
- ``candidates``: up to 10 candidates in beets' order, each with
  ``release_id``, ``artist``, ``album``, ``year``, ``label``, and
  ``distance``.
- ``copied_to``: the album directory in the beets library. Only on
  ``applied`` lines.
- ``release_id``: the MusicBrainz release that beets applied. Only on
  ``applied`` lines.
- ``reason``: ``requested_release_id`` on the ``applied`` line of an album
  whose release the ``apply_release_id`` option chose.
- ``error``: the message. Only on ``error`` lines.

Events used (beets 2.14):

- ``import_task_before_choice`` runs before beets judges the candidates.
  When the ``apply_release_id`` option is set, the handler returns the
  candidate with that release id, and beets applies it also in quiet mode.
  It returns SKIP for a task without that candidate and for a task whose
  folder is not ``apply_folder``. wn-server sets both options for
  ``wn-server match apply`` only. The handler then also adds the folder to
  beets' incremental history after the album is imported, because the apply
  run uses ``-I``, which records no history.
- ``import_task_choice`` runs after beets chose a match. The candidates and
  the original tags are recorded here, because ``task.items`` change once the
  match is applied. A task whose choice is SKIP is written at once.
- ``album_imported`` runs after the files are copied. The task that produced
  the album is found by album id and written as ``applied``.
- ``import`` runs once the import session ends. Every task still pending was
  dropped after the choice. beets decides duplicates after
  ``import_task_choice`` and sends no event for a task it skips as a
  duplicate, so a pending task with ``duplicate_action`` SKIP is written as
  ``duplicate``.

No hook raises. An internal error is written as a line with
``decision: "error"``.

The plugin also changes two beets behaviours when it loads:

- It replaces ``beets.util.extension.fix_extension`` with a function that
  returns the path unchanged. beets 2.14 calls it on every file without an
  extension. When ffprobe is on PATH, it copies the file to
  ``<name>.<ext>`` in the original folder. beets has no option to turn this
  off, and wn-server never writes to the original folders.
- When the ``acoustid_app_key`` option is set, it replaces
  ``beetsplug.chroma.API_KEY``. The chroma plugin looks up fingerprints with
  that module constant. ``acoustid.apikey`` is the user key for
  ``beet submit`` and does not change the lookup key.
"""

import json
import os
import threading
from collections import Counter

from beets.importer import Action
from beets.plugins import BeetsPlugin
from beets.util import extension as _extension

MAX_CANDIDATES = 10


def _keep_path(path_bytes, logger=None):
    """Replacement for beets.util.extension.fix_extension. It returns the
    path unchanged and writes nothing."""
    return path_bytes


# beets.importer.tasks calls extension.fix_extension through the module, so
# replacing the module attribute takes effect for every import task.
_extension.fix_extension = _keep_path


def _str(value):
    if value is None:
        return ""
    if isinstance(value, bytes):
        return os.fsdecode(value)
    return str(value)


def _int(value):
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _most_common(values):
    values = [v for v in values if v]
    if not values:
        return None
    return Counter(values).most_common(1)[0][0]


def _folder(task):
    paths = [os.path.abspath(os.fsdecode(p)) for p in (task.paths or [])]
    if not paths:
        return ""
    if len(paths) == 1:
        return paths[0]
    return os.path.commonpath(paths)


def _tags(task):
    items = list(task.items or [])
    artist = _most_common([i.albumartist or i.artist for i in items])
    album = _most_common([i.album for i in items])
    year = _most_common([_int(i.year) for i in items])
    return {
        "artist": _str(artist),
        "album": _str(album),
        "year": _int(year),
        "track_count": len(items),
    }


def _candidates(task):
    out = []
    for match in list(task.candidates or [])[:MAX_CANDIDATES]:
        info = match.info
        out.append(
            {
                "release_id": _str(getattr(info, "album_id", None)),
                "artist": _str(getattr(info, "artist", None)),
                "album": _str(getattr(info, "album", None)),
                "year": _int(getattr(info, "year", None)),
                "label": _str(getattr(info, "label", None)),
                "distance": float(match.distance.distance),
            }
        )
    return out


def _flag_name(value):
    return getattr(value, "name", None) or _str(value)


class WnCandidatesPlugin(BeetsPlugin):
    def __init__(self):
        super().__init__()
        self.config.add(
            {"output": "", "acoustid_app_key": "", "apply_release_id": "", "apply_folder": ""}
        )
        self.config["acoustid_app_key"].redact = True
        self._set_acoustid_key()
        self._lock = threading.Lock()
        # id(task) -> (task, line) for tasks that chose a match and have no
        # final decision yet.
        self._pending = {}
        # id(task) of tasks whose match on_before_choice chose.
        self._requested = set()
        self.register_listener("import_task_before_choice", self.on_before_choice)
        self.register_listener("import_task_choice", self.on_choice)
        self.register_listener("album_imported", self.on_album_imported)
        self.register_listener("import", self.on_import_end)

    def _set_acoustid_key(self):
        key = self.config["acoustid_app_key"].as_str()
        if not key:
            return
        try:
            import beetsplug.chroma as chroma

            chroma.API_KEY = key
        except Exception as exc:  # noqa: BLE001
            self._log.warning("wn_candidates: cannot set the AcoustID key: {}", exc)

    def _write(self, line):
        path = self.config["output"].as_str()
        if not path:
            self._log.warning("wn_candidates: no output file configured")
            return
        data = json.dumps(line, ensure_ascii=False)
        with self._lock:
            with open(path, "a", encoding="utf-8") as f:
                f.write(data + "\n")
                f.flush()

    def _write_error(self, folder, exc):
        try:
            self._write(
                {
                    "folder": folder,
                    "tags": {},
                    "decision": "error",
                    "candidates": [],
                    "error": f"{type(exc).__name__}: {exc}",
                }
            )
        except Exception as inner:  # noqa: BLE001
            self._log.error("wn_candidates: cannot write error line: {}", inner)

    def on_before_choice(self, session, task):
        """Return the candidate whose release id is apply_release_id, or
        SKIP. beets' choose_match takes the returned match as the decision
        before its own quiet mode judgment. Return None when the option is
        not set, so beets decides as usual."""
        try:
            release = self.config["apply_release_id"].as_str()
            if not release:
                return None
            target = self.config["apply_folder"].as_str()
            if target and os.path.normpath(_folder(task)) != os.path.normpath(target):
                return Action.SKIP
            for match in list(task.candidates or []):
                if _str(getattr(match.info, "album_id", None)) == release:
                    with self._lock:
                        self._requested.add(id(task))
                    return match
            self._log.info("wn_candidates: release {} is not a candidate for {}", release, _folder(task))
            return Action.SKIP
        except Exception as exc:  # noqa: BLE001
            self._log.error("wn_candidates: cannot choose the requested release: {}", exc)
            return Action.SKIP

    def on_choice(self, session, task):
        folder = ""
        try:
            if not getattr(task, "is_album", False):
                return
            folder = _folder(task)
            line = {
                "folder": folder,
                "tags": _tags(task),
                "decision": "",
                "candidates": _candidates(task),
            }
            flag = _flag_name(task.choice_flag)
            if flag == "APPLY":
                match = getattr(task, "match", None)
                if match is not None:
                    line["release_id"] = _str(getattr(match.info, "album_id", None))
                with self._lock:
                    if id(task) in self._requested:
                        self._requested.discard(id(task))
                        line["reason"] = "requested_release_id"
                    self._pending[id(task)] = (task, line)
                return
            line["decision"] = "skipped"
            self._write(line)
        except Exception as exc:  # noqa: BLE001
            self._write_error(folder, exc)

    def on_album_imported(self, lib, album):
        folder = ""
        try:
            found = None
            with self._lock:
                for key, (task, line) in list(self._pending.items()):
                    task_album = getattr(task, "album", None)
                    if task_album is not None and task_album.id == album.id:
                        found = line
                        found_task = task
                        del self._pending[key]
                        break
            if found is None:
                return
            if found.get("reason") == "requested_release_id":
                self._save_history(found_task)
            folder = found["folder"]
            found["decision"] = "applied"
            found["copied_to"] = os.path.abspath(os.fsdecode(album.item_dir()))
            self._write(found)
        except Exception as exc:  # noqa: BLE001
            self._write_error(folder, exc)

    def _save_history(self, task):
        """Add the task's folders to beets' incremental history. A later
        quiet import then does not offer the album again, as for an album
        that a quiet import applied."""
        try:
            task.save_history()
        except Exception as exc:  # noqa: BLE001
            self._log.warning("wn_candidates: cannot add the folder to the incremental history: {}", exc)

    def on_import_end(self, lib, paths):
        with self._lock:
            pending = list(self._pending.values())
            self._pending.clear()
        for task, line in pending:
            try:
                if _flag_name(getattr(task, "duplicate_action", None)) == "SKIP":
                    line["decision"] = "duplicate"
                elif getattr(task, "skip", False):
                    line["decision"] = "skipped"
                else:
                    line["decision"] = "error"
                    line["error"] = "beets chose a match but did not import the album"
                self._write(line)
            except Exception as exc:  # noqa: BLE001
                self._write_error(line.get("folder", ""), exc)
