#!/usr/bin/env python3
"""Exercise native release image scripts without Docker, credentials or publishing."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parent.parent
TAG_PUSH = "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')"
DIGESTS = {"amd64": "sha256:" + "a" * 64, "arm64": "sha256:" + "b" * 64}


class ReleaseImageTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = yaml.safe_load((ROOT / ".github/workflows/release.yml").read_text())
        cls.jobs = cls.workflow["jobs"]

    def step(self, job, **match):
        return next(step for step in self.jobs[job]["steps"]
                    if all(step.get(key) == value for key, value in match.items()))

    def run_shell(self, script, directory, **env):
        return subprocess.run(
            ["bash", "--noprofile", "--norc", "-euo", "pipefail", "-c", script],
            cwd=directory,
            env={"PATH": str(directory) + os.pathsep + os.defpath, **env},
            capture_output=True, text=True,
        )

    def test_owner_case_in_both_jobs(self):
        for job in ("image", "image-index"):
            script = self.step(job, id="image-name")["run"]
            for owner in ("Torutesu", "Devesh36", "lower-case"):
                with self.subTest(job=job, owner=owner), tempfile.TemporaryDirectory() as tmp:
                    output = Path(tmp) / "output"
                    result = self.run_shell(script, tmp, GITHUB_REPOSITORY_OWNER=owner,
                                            GITHUB_OUTPUT=str(output))
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output.read_text(), f"name=ghcr.io/{owner.lower()}/shogunagent\n")

    def test_native_platforms_cache_and_dependencies(self):
        image = self.jobs["image"]
        self.assertEqual(image["strategy"]["matrix"]["include"], [
            {"arch": "amd64", "runner": "ubuntu-latest"},
            {"arch": "arm64", "runner": "ubuntu-24.04-arm"},
        ])
        self.assertFalse(image["strategy"]["fail-fast"])
        self.assertEqual(image["runs-on"], "${{ matrix.runner }}")
        self.assertFalse(any("setup-qemu" in step.get("uses", "") for step in image["steps"]))
        build = self.step("image", id="build")["with"]
        self.assertEqual(build["platforms"], "linux/${{ matrix.arch }}")
        self.assertEqual(build["cache-from"], "type=gha,scope=shogunagent-image-${{ matrix.arch }}")
        self.assertEqual(build["cache-to"], "type=gha,mode=max,scope=shogunagent-image-${{ matrix.arch }}")
        self.assertNotIn("tags", build)
        self.assertEqual(build["outputs"], "type=image,name=${{ steps.image-name.outputs.name }},"
                         "push-by-digest=true,name-canonical=true,push=${{ " + TAG_PUSH + " }}")
        # A dependency on the matrix job waits for both successful native builds.
        self.assertEqual(self.jobs["image-index"]["needs"], ["image"])
        self.assertEqual(self.jobs["github-release"]["needs"], ["binaries"])

    def test_publication_gates_and_permissions(self):
        self.assertEqual(self.workflow["permissions"], {"contents": "write", "packages": "write"})
        self.assertEqual(self.jobs["image-index"]["if"], TAG_PUSH)
        self.assertEqual(self.jobs["github-release"]["if"], TAG_PUSH)
        for step in self.jobs["image"]["steps"]:
            if step.get("uses", "").startswith(("docker/login-action@", "actions/upload-artifact@")) or step.get("name") == "record the platform digest":
                self.assertEqual(step["if"], TAG_PUSH)
        # Checking the full expression pins manual dispatch as build-only, even on a tag.
        self.assertNotIn("always()", self.jobs["image-index"]["if"])

    def test_release_notes_exist_in_checked_out_tag_before_publication(self):
        import tomllib
        steps = self.jobs["github-release"]["steps"]
        release_index = next(i for i, step in enumerate(steps)
                             if step.get("uses", "").startswith("softprops/action-gh-release@"))
        self.assertTrue(any(step.get("uses", "").startswith("actions/checkout@")
                            for step in steps[:release_index]),
                        "release notes are source files: this job needs its own checkout")
        settings = steps[release_index]["with"]
        self.assertEqual(settings["body_path"], "docs/releases/${{ github.ref_name }}.md")
        version = tomllib.loads((ROOT / "crates/shogun-core/Cargo.toml").read_text())["package"]["version"]
        notes = ROOT / settings["body_path"].replace("${{ github.ref_name }}", "v" + version)
        self.assertTrue(notes.is_file(), "the shipping version needs release notes")
        self.assertTrue(notes.read_text().startswith("# ShogunAgent v" + version + "\n"))

    def test_record_digest_validates_before_writing(self):
        script = self.step("image", name="record the platform digest")["run"]
        for digest in (DIGESTS["amd64"], "", "sha256:" + "z" * 64, "sha256:" + "a" * 63):
            with self.subTest(digest=digest), tempfile.TemporaryDirectory() as tmp:
                result = self.run_shell(script, tmp, IMAGE_DIGEST=digest, IMAGE_ARCH="amd64")
                output = Path(tmp) / "digests/amd64.digest"
                if digest == DIGESTS["amd64"]:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output.read_text(), digest + "\n")
                else:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(output.exists())

    def run_index(self, digests, docker_exit=0):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            (directory / "digests").mkdir()
            for arch, digest in digests.items():
                (directory / "digests" / f"{arch}.digest").write_text(digest + "\n")
            # Capture argv exactly; never invoke a real Docker binary or a network client.
            docker = directory / "docker"
            docker.write_text("#!/usr/bin/python3\nimport json, os, sys\n"
                              "with open(os.environ['DOCKER_LOG'], 'a') as f:\n"
                              "    f.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                              "sys.exit(int(os.environ['DOCKER_EXIT']))\n")
            docker.chmod(0o700)
            log = directory / "calls"
            result = self.run_shell(self.step("image-index", id="publish-index")["run"], tmp,
                                    IMAGE_NAME="ghcr.io/torutesu/shogunagent", RELEASE_TAG="v0.1.0",
                                    DOCKER_LOG=str(log), DOCKER_EXIT=str(docker_exit))
            calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
            return result, calls

    def test_index_requires_both_valid_digests_before_any_docker_call(self):
        for digests in ({}, {"amd64": DIGESTS["amd64"]}, {"arm64": DIGESTS["arm64"]},
                        {**DIGESTS, "arm64": "invalid"}, {**DIGESTS, "amd64": "sha256:" + "a" * 63},
                        {**DIGESTS, "arm64": DIGESTS["arm64"] + "\nextra"}):
            with self.subTest(digests=digests):
                result, calls = self.run_index(digests)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, [])

    def test_index_publishes_two_references_and_inspects_tags(self):
        result, calls = self.run_index(DIGESTS)
        self.assertEqual(result.returncode, 0, result.stderr)
        name = "ghcr.io/torutesu/shogunagent"
        self.assertEqual(calls, [
            ["buildx", "imagetools", "create", "--tag", name + ":v0.1.0", "--tag", name + ":latest",
             name + "@" + DIGESTS["amd64"], name + "@" + DIGESTS["arm64"]],
            ["buildx", "imagetools", "inspect", name + ":v0.1.0"],
            ["buildx", "imagetools", "inspect", name + ":latest"],
        ])

    def test_publication_error_stops_before_inspection(self):
        result, calls = self.run_index(DIGESTS, docker_exit=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls), 1)


if __name__ == "__main__":
    unittest.main()
