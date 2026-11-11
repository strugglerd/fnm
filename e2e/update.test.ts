import { mkdirSync, readdirSync, realpathSync, rmSync } from "node:fs"
import { createServer } from "node:http"
import type { AddressInfo } from "node:net"
import { join } from "node:path"
import { script } from "./shellcode/script.js"
import { Bash, Fish, PowerShell, Zsh } from "./shellcode/shells.js"
import describe from "./describe.js"
import testTmpDir from "./shellcode/test-tmp-dir.js"

const fnmDir = () => join(testTmpDir(), "fnm")

function installedVersionsOf(major: number): string[] {
  return readdirSync(join(fnmDir(), "node-versions"))
    .filter((name) => name.startsWith(`v${major}.`))
    .sort()
}

function aliasTarget(name: string): string {
  return realpathSync(join(fnmDir(), "aliases", name))
}

// `script()` only resets aliases, so drop installed versions for a hermetic run.
function resetFnmDir(): void {
  rmSync(join(fnmDir(), "node-versions"), { recursive: true, force: true })
}

for (const shell of [Bash, Zsh, Fish, PowerShell]) {
  describe(shell, () => {
    test(`--dry-run reports an update without installing it`, async () => {
      if (!shell.currentlySupported()) return
      resetFnmDir()

      await script(shell)
        .then(shell.env({}))
        .then(shell.call("fnm", ["install", "20.11.0"]))
        .then(
          shell.scriptOutputContains(
            shell.call("fnm", ["update", "--dry-run", "20"]),
            '"update available"',
          ),
        )
        .takeSnapshot(shell)
        .execute(shell)

      expect(installedVersionsOf(20)).toEqual(["v20.11.0"])
    })

    test(`moves aliases to the updated version and keeps the old one`, async () => {
      if (!shell.currentlySupported()) return
      resetFnmDir()

      await script(shell)
        .then(shell.env({}))
        .then(shell.call("fnm", ["install", "20.11.0"]))
        .then(shell.call("fnm", ["alias", "20.11.0", "hello"]))
        .then(shell.call("fnm", ["update", "20"]))
        .takeSnapshot(shell)
        .execute(shell)

      const defaultTarget = aliasTarget("default")
      expect(aliasTarget("hello")).toBe(defaultTarget)
      expect(defaultTarget).not.toContain("v20.11.0")
      expect(installedVersionsOf(20)).toContain("v20.11.0")
      expect(installedVersionsOf(20)).toHaveLength(2)
    })

    test(`--prune removes the superseded version`, async () => {
      if (!shell.currentlySupported()) return
      resetFnmDir()

      await script(shell)
        .then(shell.env({}))
        .then(shell.call("fnm", ["install", "20.11.0"]))
        .then(shell.call("fnm", ["update", "--prune", "20"]))
        .takeSnapshot(shell)
        .execute(shell)

      const versions = installedVersionsOf(20)
      expect(versions).toHaveLength(1)
      expect(versions[0]).not.toBe("v20.11.0")
      expect(aliasTarget("default")).toContain(versions[0])
    })

    test(`keeps prerelease installs in their own channel`, async () => {
      if (!shell.currentlySupported()) return
      resetFnmDir()

      // nodejs.org's published index only ever carries a single prerelease, so
      // serve a synthetic one to exercise the channel logic end to end.
      const index = [
        { version: "v22.0.0-beta.9", lts: false },
        { version: "v22.0.0-rc.1", lts: false },
        { version: "v22.0.0-rc.2", lts: false },
        { version: "v22.0.0", lts: false },
        { version: "v22.1.0", lts: false },
      ]
      const server = createServer((req, res) => {
        if (req.url?.startsWith("/index.json")) {
          res.writeHead(200, { "content-type": "application/json" })
          res.end(JSON.stringify(index))
          return
        }
        res.writeHead(404)
        res.end()
      })
      await new Promise<void>((resolve) => {
        server.listen(0, "127.0.0.1", resolve)
      })
      const { port } = server.address() as AddressInfo

      // Seed the installed prerelease, the update check only reads the directory.
      mkdirSync(
        join(fnmDir(), "node-versions", "v22.0.0-rc.1", "installation"),
        { recursive: true },
      )

      try {
        await script(shell)
          .then(shell.env({}))
          .then(
            shell.scriptOutputContains(
              shell.call("fnm", [
                "update",
                "--dry-run",
                "--node-dist-mirror",
                `http://127.0.0.1:${port}`,
              ]),
              '"v22.0.0-rc.2"',
            ),
          )
          .then(
            shell.scriptOutputContains(
              shell.call("fnm", [
                "update",
                "--dry-run",
                "--node-dist-mirror",
                `http://127.0.0.1:${port}`,
              ]),
              '"stable v22.1.0 released"',
            ),
          )
          .execute(shell)
      } finally {
        await new Promise<void>((resolve) => {
          server.close(() => {
            resolve()
          })
        })
      }
    })
  })
}
