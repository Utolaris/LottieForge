const std = @import("std");

pub fn build(b: *std.Build) void {
    const zig_exe = b.graph.zig_exe;

    const win_x64 = b.step("win-x64", "Cross-compile tgs-convert.exe (Windows x64) with zig");

    const toolchain = toolchain_dir();
    const setup = setup_toolchain(b, zig_exe, toolchain);

    const rustup = b.addSystemCommand(&.{ "rustup", "target", "add", "x86_64-pc-windows-gnu" });

    const cargo = b.addSystemCommand(&.{
        "cargo", "build", "--target", "x86_64-pc-windows-gnu", "--release",
    });
    cargo.setCwd(b.path("tgs-convert"));
    cargo.setEnvironmentVariable("CC_x86_64_pc_windows_gnu", b.pathJoin(&.{ toolchain, "cc.sh" }));
    cargo.setEnvironmentVariable("CXX_x86_64_pc_windows_gnu", b.pathJoin(&.{ toolchain, "cxx.sh" }));
    cargo.setEnvironmentVariable("AR_x86_64_pc_windows_gnu", b.pathJoin(&.{ toolchain, "ar.sh" }));
    cargo.setEnvironmentVariable("RANLIB_x86_64_pc_windows_gnu", b.pathJoin(&.{ toolchain, "ranlib.sh" }));
    cargo.setEnvironmentVariable(
        "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER",
        b.pathJoin(&.{ toolchain, "linker.sh" }),
    );
    cargo.setEnvironmentVariable("ZIG_EXE", zig_exe);
    if (std.c.getenv("PATH")) |path_ptr| {
        const path = std.mem.span(path_ptr);
        cargo.setEnvironmentVariable("PATH", b.fmt("{s}:{s}", .{ toolchain, path }));
    }
    cargo.step.dependOn(&rustup.step);
    cargo.step.dependOn(&setup.step);
    win_x64.dependOn(&cargo.step);

    const install = b.addInstallFileWithDir(
        b.path("tgs-convert/target/x86_64-pc-windows-gnu/release/tgs-convert.exe"),
        .bin,
        "tgs-convert.exe",
    );
    install.step.dependOn(&cargo.step);
    b.getInstallStep().dependOn(&install.step);
    win_x64.dependOn(&install.step);

    const mac = b.step("mac", "Build the native macOS binary");
    const cargo_mac = b.addSystemCommand(&.{ "cargo", "build", "--release" });
    cargo_mac.setCwd(b.path("tgs-convert"));
    mac.dependOn(&cargo_mac.step);
    const install_mac = b.addInstallFileWithDir(
        b.path("tgs-convert/target/release/tgs-convert"),
        .bin,
        "tgs-convert",
    );
    install_mac.step.dependOn(&cargo_mac.step);
    mac.dependOn(&install_mac.step);

    const test_step = b.step("test", "Run cargo tests on the host");
    const cargo_test = b.addSystemCommand(&.{ "cargo", "test" });
    cargo_test.setCwd(b.path("tgs-convert"));
    test_step.dependOn(&cargo_test.step);
}

fn toolchain_dir() []const u8 {
    const alloc = std.heap.page_allocator;
    const base = if (std.c.getenv("TMPDIR")) |tmp| std.mem.span(tmp) else "/tmp";
    return std.fs.path.join(alloc, &.{ base, "tgsconvert-zig-win" }) catch unreachable;
}

fn setup_toolchain(b: *std.Build, zig_exe: []const u8, dir: []const u8) *std.Build.Step.Run {
    const script =
        \\set -e
        \\dir="$TOOLCHAIN_DIR"
        \\mkdir -p "$dir"
        \\
        \\cat > "$dir/cc.sh" <<'EOF'
        \\#!/bin/sh
        \\args=""
        \\for arg in "$@"; do
        \\  case "$arg" in
        \\    --target=x86_64-pc-windows-gnu) continue ;;
        \\  esac
        \\  args="$args $(printf '%s' "$arg" | sed "s/['\"\\]/\\\\&/g")"
        \\done
        \\eval "exec \"$ZIG_EXE\" cc -target x86_64-windows-gnu $args"
        \\EOF
        \\chmod +x "$dir/cc.sh"
        \\
        \\cat > "$dir/cxx.sh" <<'EOF'
        \\#!/bin/sh
        \\args=""
        \\for arg in "$@"; do
        \\  case "$arg" in
        \\    --target=x86_64-pc-windows-gnu) continue ;;
        \\  esac
        \\  args="$args $(printf '%s' "$arg" | sed "s/['\"\\]/\\\\&/g")"
        \\done
        \\eval "exec \"$ZIG_EXE\" c++ -target x86_64-windows-gnu $args"
        \\EOF
        \\chmod +x "$dir/cxx.sh"
        \\
        \\cat > "$dir/linker.sh" <<'EOF'
        \\#!/bin/sh
        \\here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
        \\args=""
        \\for arg in "$@"; do
        \\  case "$arg" in
        \\    --target=x86_64-pc-windows-gnu|-Wl,--dynamicbase|-Wl,--disable-auto-image-base|-Wl,--nxcompat) continue ;;
        \\    -lmsvcrt) args="$args '$here/msvcrt.lib'"; continue ;;
        \\    -l:libpthread.a) args="$args -lpthread"; continue ;;
        \\    -lgcc_eh) args="$args '$here/frame_stub.o'"; continue ;;
        \\    -lgcc) continue ;;
        \\  esac
        \\  args="$args $(printf '%s' "$arg" | sed "s/['\"\\]/\\\\&/g")"
        \\done
        \\eval "exec \"$ZIG_EXE\" cc -target x86_64-windows-gnu -L'$here' $args"
        \\EOF
        \\chmod +x "$dir/linker.sh"
        \\
        \\cat > "$dir/ar.sh" <<'EOF'
        \\#!/bin/sh
        \\if [ -n "$LLVM_AR" ] && [ -x "$LLVM_AR" ]; then exec "$LLVM_AR" "$@"; fi
        \\for p in /opt/homebrew/opt/llvm*/bin/llvm-ar /usr/local/opt/llvm*/bin/llvm-ar; do
        \\  if [ -x "$p" ]; then exec "$p" "$@"; fi
        \\done
        \\if command -v x86_64-w64-mingw32-ar >/dev/null 2>&1; then exec x86_64-w64-mingw32-ar "$@"; fi
        \\exec "$ZIG_EXE" ar "$@"
        \\EOF
        \\chmod +x "$dir/ar.sh"
        \\
        \\cat > "$dir/ranlib.sh" <<'EOF'
        \\#!/bin/sh
        \\exec "$ZIG_EXE" ranlib "$@"
        \\EOF
        \\chmod +x "$dir/ranlib.sh"
        \\
        \\cat > "$dir/x86_64-w64-mingw32-dlltool" <<'EOF'
        \\#!/bin/sh
        \\exec "$ZIG_EXE" dlltool "$@"
        \\EOF
        \\chmod +x "$dir/x86_64-w64-mingw32-dlltool"
        \\
        \\cat > "$dir/cmake" <<'EOF'
        \\#!/bin/sh
        \\here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
        \\stripped=""
        \\IFS=:
        \\for entry in $PATH; do
        \\  if [ "$entry" = "$here" ]; then continue; fi
        \\  if [ -n "$stripped" ]; then stripped="$stripped:"; fi
        \\  stripped="$stripped$entry"
        \\done
        \\PATH="$stripped"
        \\export PATH
        \\real_cmake=$(command -v cmake)
        \\if [ "$1" = "--build" ]; then
        \\  exec "$real_cmake" "$@"
        \\fi
        \\exec "$real_cmake" \
        \\  -DCMAKE_SYSTEM_NAME=Windows \
        \\  -DCMAKE_SYSTEM_PROCESSOR=x86_64 \
        \\  -DCMAKE_C_COMPILER="$here/cc.sh" \
        \\  -DCMAKE_CXX_COMPILER="$here/cxx.sh" \
        \\  -DCMAKE_AR="$here/ar.sh" \
        \\  -DCMAKE_RANLIB="$here/ranlib.sh" \
        \\  -DCMAKE_CXX_FLAGS=-DLOT_BUILD=1 \
        \\  "$@"
        \\EOF
        \\chmod +x "$dir/cmake"
        \\
        \\cat > "$dir/frame_stub.c" <<'EOF'
        \\void __register_frame_info(void *begin, void *ob);
        \\void __deregister_frame_info(const void *begin);
        \\void __register_frame_info(void *begin, void *ob) {
        \\    (void)begin;
        \\    (void)ob;
        \\}
        \\void __deregister_frame_info(const void *begin) { (void)begin; }
        \\EOF
        \\"$ZIG_EXE" cc -target x86_64-windows-gnu -c "$dir/frame_stub.c" -o "$dir/frame_stub.o"
        \\
        \\if [ -f /opt/homebrew/opt/mingw-w64/toolchain-x86_64/x86_64-w64-mingw32/lib/libmsvcrt.a ]; then
        \\  cp /opt/homebrew/opt/mingw-w64/toolchain-x86_64/x86_64-w64-mingw32/lib/libmsvcrt.a "$dir/msvcrt.lib"
        \\elif [ -f /usr/local/opt/mingw-w64/toolchain-x86_64/x86_64-w64-mingw32/lib/libmsvcrt.a ]; then
        \\  cp /usr/local/opt/mingw-w64/toolchain-x86_64/x86_64-w64-mingw32/lib/libmsvcrt.a "$dir/msvcrt.lib"
        \\else
        \\  echo "warning: mingw-w64 libmsvcrt.a not found; run: brew install mingw-w64" >&2
        \\fi
        \\
    ;
    const run = b.addSystemCommand(&.{ "sh", "-c", script });
    run.setEnvironmentVariable("TOOLCHAIN_DIR", dir);
    run.setEnvironmentVariable("ZIG_EXE", zig_exe);
    return run;
}
