#!/bin/bash
# Patch libfyaml for mingw/Windows cross-compilation
# Adds #ifdef guards around POSIX-only headers and provides Windows alternatives

set -e

DIR="${1:-.}"

echo "Patching libfyaml in $DIR for mingw compatibility..."

# Create a Windows compatibility header
cat > "$DIR/src/fy-win32-compat.h" << 'HEADER'
#ifndef FY_WIN32_COMPAT_H
#define FY_WIN32_COMPAT_H

#ifdef _WIN32

#include <malloc.h>   /* alloca on mingw */
#include <windows.h>
#include <io.h>

/* mmap stubs using Windows VirtualAlloc */
#define PROT_READ     0x1
#define PROT_WRITE    0x2
#define MAP_PRIVATE   0x02
#define MAP_ANONYMOUS 0x20
#define MAP_FAILED    ((void*)-1)

static inline void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset) {
    (void)addr; (void)prot; (void)flags; (void)fd; (void)offset;
    void *p = VirtualAlloc(NULL, length, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    return p ? p : MAP_FAILED;
}

static inline int munmap(void *addr, size_t length) {
    (void)length;
    return VirtualFree(addr, 0, MEM_RELEASE) ? 0 : -1;
}

/* termios stubs */
struct termios { int c_lflag; int c_cc[20]; };
#define ECHO 0
#define ICANON 0
#define TCSANOW 0
#define VMIN 0
#define VTIME 0
static inline int tcgetattr(int fd, struct termios *t) { (void)fd; (void)t; return -1; }
static inline int tcsetattr(int fd, int opt, const struct termios *t) { (void)fd; (void)opt; (void)t; return -1; }

/* ioctl stub */
#define TIOCGWINSZ 0
struct winsize { unsigned short ws_row; unsigned short ws_col; };
static inline int ioctl(int fd, unsigned long req, ...) { (void)fd; (void)req; return -1; }

/* select - mingw has winsock2 select but not sys/select.h */
#include <winsock2.h>

/* sysmacros */
#ifndef major
#define major(dev) (0)
#define minor(dev) (0)
#define makedev(maj, min) (0)
#endif

/* syscall stub */
#define SYS_gettid 0
static inline long syscall(long n, ...) { (void)n; return (long)GetCurrentThreadId(); }

#endif /* _WIN32 */
#endif /* FY_WIN32_COMPAT_H */
HEADER

# Replace POSIX includes with guarded versions
find "$DIR/src" -name '*.c' -o -name '*.h' | while read f; do
    # Add compat header include after first system include
    if grep -q 'sys/mman.h\|alloca.h\|termios.h\|sys/ioctl.h\|sys/select.h\|sys/syscall.h\|sys/sysmacros.h' "$f"; then
        # Wrap alloca.h
        sed -i 's|^#include <alloca.h>|#ifdef _WIN32\n#include "fy-win32-compat.h"\n#else\n#include <alloca.h>\n#endif|' "$f"

        # Wrap sys/mman.h
        sed -i 's|^#include <sys/mman.h>|#ifdef _WIN32\n#include "fy-win32-compat.h"\n#else\n#include <sys/mman.h>\n#endif|' "$f"

        # Wrap termios.h
        sed -i 's|^#include <termios.h>|#ifdef _WIN32\n#include "fy-win32-compat.h"\n#else\n#include <termios.h>\n#endif|' "$f"

        # Wrap sys/ioctl.h
        sed -i 's|^#include <sys/ioctl.h>|#ifdef _WIN32\n#include "fy-win32-compat.h"\n#else\n#include <sys/ioctl.h>\n#endif|' "$f"

        # Wrap sys/select.h
        sed -i 's|^#include <sys/select.h>|#ifdef _WIN32\n#include "fy-win32-compat.h"\n#else\n#include <sys/select.h>\n#endif|' "$f"

        # Wrap sys/syscall.h
        sed -i 's|^#include <sys/syscall.h>|#ifdef _WIN32\n#include "fy-win32-compat.h"\n#else\n#include <sys/syscall.h>\n#endif|' "$f"

        # Wrap sys/sysmacros.h
        sed -i 's|^#include <sys/sysmacros.h>|#ifdef _WIN32\n#include "fy-win32-compat.h"\n#else\n#include <sys/sysmacros.h>\n#endif|' "$f"
    fi
done

echo "Patching complete."
