"""omni-embed: image search for omni (`img <description>`), using embeddinggemma-2.

    omni-embed.py serve        answer queries on stdin, one JSON line each:
                               {"q": "<description>", "k": 12} -> {"q": ..., "hits": [{"p", "t", "s"}]}
                               Loads only the text side. Exits after IDLE_EXIT s with no query.
    omni-embed.py crawl [--fast] [--add DIR [--recursive]]
                               embed new or changed images in the configured folders (DIR is
                               added first; --recursive takes its subdirectories too). Loads the
                               vision encoder only when there is something to embed, and exits
                               when done. Progress on stdout. Without --fast it stays out of the
                               way (low priority, one image at a time); --fast (Index now) runs
                               at full speed when the machine has the room.

Runs in its own venv (onnxruntime-gpu brings its own CUDA and cuDNN), started by omni.
"""

import fcntl
import hashlib
import json
import os
import select
import sys
import time
from pathlib import Path

HOME = Path.home()
DATA = Path(os.environ.get("XDG_DATA_HOME", HOME / ".local/share")) / "omni"
CONFIG = Path(os.environ.get("XDG_CONFIG_HOME", HOME / ".config")) / "omni"
CACHE = Path(os.environ.get("XDG_CACHE_HOME", HOME / ".cache")) / "omni"
MODEL_REPO = "onnx-community/embeddinggemma-2-ONNX"
MODEL_DIR = DATA / "models/embeddinggemma-2"
MODEL_FILES = ["onnx/model_fp16.onnx", "onnx/model_fp16.onnx_data",
               "onnx/vision_encoder_fp16.onnx", "onnx/vision_encoder_fp16.onnx_data",
               "*.json", "*.jinja"]
INDEX_DIR = DATA / "images"
INDEX = INDEX_DIR / "index.npz"
STATUS = INDEX_DIR / "status.json"
LOCK = INDEX_DIR / "crawl.lock"
THUMBS = CACHE / "thumbs-160"  # versioned by size: a new size regenerates them
FOLDERS = CONFIG / "image-folders"

QUERY_PREFIX = "task: search result | query: "
DOC_PREFIX = "title: none | text: "  # measured better than a bare image for text->image search
IMAGE_EXT = {".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp"}
MAX_BYTES = 64 << 20
THUMB_PX = 160
IDLE_EXIT = 10.0
SAVE_EVERY = 50


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


# image-folders: one folder per line; a trailing "/**" also takes its subdirectories.
# No file: ~/Pictures with its subdirectories.

def read_folders():
    """[(Path, recursive)] as written, existing or not."""
    try:
        lines = [l.strip() for l in FOLDERS.read_text().splitlines()]
    except FileNotFoundError:
        return [(HOME / "Pictures", True)]
    out = []
    for l in lines:
        if not l or l.startswith("#"):
            continue
        rec = l.endswith("/**")
        out.append((Path(os.path.expanduser(l[:-3] if rec else l)), rec))
    return out


def folders():
    return [(d, r) for d, r in read_folders() if d.is_dir()]


def add_folder(d, recursive):
    """Add (or widen to subdirectories) a folder. Returns "new", "deeper" or "same"."""
    d = Path(os.path.expanduser(d)).resolve()
    current = [(f.resolve(), r) for f, r in read_folders()]
    state = "new"
    for i, (f, r) in enumerate(current):
        if f == d:
            state = "deeper" if recursive and not r else "same"
            current[i] = (f, r or recursive)  # never narrowed by re-adding without the toggle
            break
    else:
        current.append((d, recursive))
    if state != "same":
        def show(f, r):
            s = str(f)
            if s == str(HOME) or s.startswith(str(HOME) + "/"):
                s = "~" + s[len(str(HOME)):]
            return s + ("/**" if r else "")
        FOLDERS.parent.mkdir(parents=True, exist_ok=True)
        tmp = FOLDERS.with_suffix(".new")
        tmp.write_text("".join(show(f, r) + "\n" for f, r in current))
        tmp.replace(FOLDERS)
    return state


def thumb_path(path):
    return THUMBS / (hashlib.sha1(path.encode()).hexdigest() + ".png")


# ---- index: one .npz (paths, mtimes, sizes, float16 vectors), replaced atomically ----

def load_index():
    import numpy as np
    try:
        with np.load(INDEX, allow_pickle=False) as z:
            return {"paths": list(z["paths"]), "mtimes": z["mtimes"].tolist(),
                    "sizes": z["sizes"].tolist(), "vecs": z["vecs"],
                    "bad": dict(zip(list(z["bad_paths"]), z["bad_mtimes"].tolist()))}
    except (FileNotFoundError, KeyError, ValueError, OSError):
        return {"paths": [], "mtimes": [], "sizes": [], "vecs": np.zeros((0, 768), np.float16), "bad": {}}


def save_index(paths, mtimes, sizes, vecs, bad):
    import numpy as np
    INDEX_DIR.mkdir(parents=True, exist_ok=True)
    tmp = INDEX.with_name("index.new.npz")
    np.savez(tmp, paths=np.array(paths, dtype=str), mtimes=np.array(mtimes, dtype=np.int64),
             sizes=np.array(sizes, dtype=np.int64),
             vecs=np.asarray(vecs, np.float16).reshape(-1, 768),
             bad_paths=np.array(list(bad), dtype=str), bad_mtimes=np.array(list(bad.values()), dtype=np.int64))
    tmp.replace(INDEX)
    STATUS.write_text(json.dumps({"count": len(paths), "finished": int(time.time())}))


# ---- model ----

def session(name):
    import onnxruntime as ort
    try:
        ort.preload_dlls()  # the pip CUDA/cuDNN libraries
    except Exception:
        pass
    opts = ort.SessionOptions()
    opts.log_severity_level = 3
    providers = [p for p in ("CUDAExecutionProvider", "CPUExecutionProvider") if p in ort.get_available_providers()]
    return ort.InferenceSession(str(MODEL_DIR / "onnx" / name), opts, providers=providers)


def ensure_model():
    need = ["onnx/model_fp16.onnx_data", "onnx/vision_encoder_fp16.onnx_data", "tokenizer.json", "processor_config.json"]
    if all((MODEL_DIR / n).exists() for n in need):
        return
    emit({"event": "downloading"})
    from huggingface_hub import snapshot_download
    snapshot_download(MODEL_REPO, allow_patterns=MODEL_FILES, local_dir=str(MODEL_DIR))


class Text:
    """The text side: tokenizer + the main transformer. Images also go through it."""

    def __init__(self):
        import numpy as np
        from tokenizers import Tokenizer
        self.np = np
        self.tok = Tokenizer.from_file(str(MODEL_DIR / "tokenizer.json"))
        self.sess = session("model_fp16.onnx")
        self.none = np.zeros((0, 512), np.float32)

    def embed(self, text, image_features=None):
        np = self.np
        ids = np.array([self.tok.encode(text).ids], np.int64)
        feeds = {"input_ids": ids, "attention_mask": np.ones_like(ids),
                 "image_features": self.none if image_features is None else image_features,
                 "video_features": self.none, "audio_features": self.none}
        v = self.sess.run(["sentence_embedding"], feeds)[0][0].astype(np.float32)
        return v / (np.linalg.norm(v) or 1.0)


# ---- serve ----

def serve():
    import numpy as np
    if not (MODEL_DIR / "onnx/model_fp16.onnx_data").exists():
        text = None
    else:
        text = Text()
    index, index_mtime = None, -1  # -1: load on the first query even with no index file
    emit({"event": "ready"})
    while True:
        r, _, _ = select.select([sys.stdin], [], [], IDLE_EXIT)
        if not r:
            return  # idle: free the GPU and RAM; omni starts us again on the next query
        line = sys.stdin.readline()
        if not line:
            return
        try:
            req = json.loads(line)
        except ValueError:
            continue
        q, k = str(req.get("q", "")), int(req.get("k", 12))
        try:
            try:
                m = INDEX.stat().st_mtime_ns
            except FileNotFoundError:
                m = None
            if m != index_mtime:
                index, index_mtime = load_index(), m
                vecs = index["vecs"].astype(np.float32)
            if text is None or not index["paths"]:
                emit({"q": q, "hits": [], "empty": True})
                continue
            qv = text.embed(QUERY_PREFIX + q)
            sims = vecs @ qv
            hits = []
            for i in np.argsort(-sims):
                p = index["paths"][i]
                if not os.path.exists(p):
                    continue
                t = thumb_path(p)
                hits.append({"p": p, "t": str(t) if t.exists() else "", "s": round(float(sims[i]), 4)})
                if len(hits) >= k:
                    break
            emit({"q": q, "hits": hits})
        except Exception as e:  # one bad query must not take the helper (and its loaded model) down
            print(f"omni-embed: query {q!r} failed: {e}", file=sys.stderr)
            emit({"q": q, "hits": [], "error": str(e)})


# ---- crawl ----

def candidates():
    out = {}
    for root, recursive in folders():
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if not d.startswith(".")] if recursive else []
            for f in filenames:
                if f.startswith(".") or os.path.splitext(f)[1].lower() not in IMAGE_EXT:
                    continue
                p = os.path.join(dirpath, f)
                try:
                    st = os.stat(p)
                except OSError:
                    continue
                if 0 < st.st_size <= MAX_BYTES:
                    out[p] = (st.st_mtime_ns, st.st_size)
    return out


def make_thumb(im, p):
    th = im.copy()
    th.thumbnail((THUMB_PX, THUMB_PX))
    th.save(thumb_path(p))


def fill_thumbs(paths):
    """Thumbnails missing for indexed images (cleared cache, new size): PIL only, no model."""
    from PIL import Image, ImageOps
    THUMBS.mkdir(parents=True, exist_ok=True)
    for p in paths:
        if thumb_path(p).exists():
            continue
        try:
            with Image.open(p) as im:
                im.seek(0)
                make_thumb(ImageOps.exif_transpose(im).convert("RGB"), p)
        except Exception as e:
            print(f"omni-embed: no thumbnail for {p}: {e}", file=sys.stderr)


def room_to_go_fast():
    """Index now: full speed only with free VRAM (vision + text ~1.2 GB) and RAM to spare.
    Returns worker threads for decoding/preprocessing, or 0 to stay gentle."""
    try:
        import subprocess
        out = subprocess.run(["nvidia-smi", "--query-gpu=memory.free", "--format=csv,noheader,nounits"],
                             capture_output=True, text=True, timeout=5).stdout.split()
        if out and int(out[0]) < 1800:
            return 0
    except Exception:
        pass  # no nvidia-smi: ORT falls back on its own; judge by RAM and CPUs
    try:
        avail = next(int(l.split()[1]) for l in open("/proc/meminfo") if l.startswith("MemAvailable:"))
        if avail < 3 << 20:  # kB: under 3 GB free
            return 0
    except Exception:
        pass
    return max(1, min(4, (os.cpu_count() or 2) // 4))


def crawl(add=None, recursive=False, fast=False):
    import numpy as np
    INDEX_DIR.mkdir(parents=True, exist_ok=True)
    lock = open(LOCK, "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        emit({"event": "busy"})  # another crawl is running; it will pick the new folder up next time
        if add:
            add_folder(add, recursive)
        return
    workers = room_to_go_fast() if fast else 0
    if not workers:
        os.nice(10)
    if add:
        emit({"event": "folder", "state": add_folder(add, recursive)})
    found = candidates()
    old = load_index()
    paths, mtimes, sizes, vecs = [], [], [], []
    for i, p in enumerate(old["paths"]):
        if found.get(p) == (old["mtimes"][i], old["sizes"][i]):
            paths.append(p); mtimes.append(old["mtimes"][i]); sizes.append(old["sizes"][i]); vecs.append(old["vecs"][i])
    bad = {p: m for p, m in old["bad"].items() if p in found and found[p][0] == m}
    known = set(paths)
    todo = [p for p in found if p not in known and p not in bad]
    removed = len(old["paths"]) - len(paths)
    fill_thumbs(paths)
    if not todo:
        if removed or len(bad) != len(old["bad"]):
            save_index(paths, mtimes, sizes, vecs, bad)
        elif not STATUS.exists():
            save_index(paths, mtimes, sizes, vecs, bad)
        emit({"event": "done", "count": len(paths), "added": 0, "removed": removed})
        return

    ensure_model()
    from PIL import Image, ImageOps
    from transformers.models.gemma4.image_processing_pil_gemma4 import Gemma4ImageProcessorPil
    import transformers
    transformers.logging.set_verbosity_error()
    text = Text()
    vision = session("vision_encoder_fp16.onnx")
    proc = Gemma4ImageProcessorPil.from_pretrained(str(MODEL_DIR))
    boi, img_tok, eoi = "<|image>", "<|image|>", "<image|>"
    THUMBS.mkdir(parents=True, exist_ok=True)
    def prepare(p):
        """CPU half (decode, thumbnail, patches): runs ahead on worker threads with --fast."""
        with Image.open(p) as im:
            im.seek(0)
            im = ImageOps.exif_transpose(im).convert("RGB")  # camera photos stand upright
        make_thumb(im, p)
        return proc(images=[im], return_tensors="np")

    def prepared():
        """(path, processor output or exception), in order. With workers, the GPU never waits
        on PIL: a bounded window of images is decoded while the current one is encoded."""
        if not workers:
            for p in todo:
                try:
                    yield p, prepare(p)
                except Exception as e:
                    yield p, e
            return
        from concurrent.futures import ThreadPoolExecutor
        from collections import deque
        with ThreadPoolExecutor(workers) as pool:
            window = deque()
            it = iter(todo)
            for p in it:
                window.append((p, pool.submit(prepare, p)))
                if len(window) >= workers * 2:
                    break
            while window:
                p, fut = window.popleft()
                nxt = next(it, None)
                if nxt is not None:
                    window.append((nxt, pool.submit(prepare, nxt)))
                try:
                    yield p, fut.result()
                except Exception as e:
                    yield p, e

    added = 0
    emit({"event": "progress", "done": 0, "total": len(todo), "fast": bool(workers)})
    for n, (p, o) in enumerate(prepared(), 1):
        try:
            if isinstance(o, Exception):
                raise o
            feats = vision.run(None, {"pixel_values": o["pixel_values"].astype(np.float32),
                                      "pixel_position_ids": o["image_position_ids"].astype(np.int64)})[0]
            soft = int(o["num_soft_tokens_per_image"][0])
            v = text.embed(DOC_PREFIX + boi + img_tok * soft + eoi, feats[:soft].astype(np.float32))
            paths.append(p); mtimes.append(found[p][0]); sizes.append(found[p][1]); vecs.append(v.astype(np.float16))
            added += 1
        except Exception as e:  # unreadable or odd image: skip it until it changes
            bad[p] = found[p][0]
            print(f"omni-embed: skipped {p}: {e}", file=sys.stderr)
        if n % SAVE_EVERY == 0:
            save_index(paths, mtimes, sizes, vecs, bad)
        emit({"event": "progress", "done": n, "total": len(todo)})
    save_index(paths, mtimes, sizes, vecs, bad)
    emit({"event": "done", "count": len(paths), "added": added, "removed": removed})


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "serve":
        serve()
    elif cmd == "crawl":
        args = sys.argv[2:]
        add = args[args.index("--add") + 1] if "--add" in args else None
        crawl(add, recursive="--recursive" in args, fast="--fast" in args)
    else:
        print(__doc__, file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
