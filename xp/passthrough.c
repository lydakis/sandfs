// Minimal passthrough FUSE file system (FUSE 2.x high-level API), for measuring what a
// mount transport costs on macOS: every call goes straight to the backing folder.
// Usage: passthrough BACKING MOUNTPOINT [fuse options]
#define FUSE_USE_VERSION 26
#include <fuse.h>
#include <errno.h>
#include <fcntl.h>
#include <dirent.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/statvfs.h>

static char root[PATH_MAX];
#define P(path) char real[PATH_MAX]; snprintf(real, sizeof real, "%s%s", root, path)
#define R(x) do { if ((x) == -1) return -errno; return 0; } while (0)

static int pt_getattr(const char *path, struct stat *st) { P(path); R(lstat(real, st)); }
static int pt_fgetattr(const char *path, struct stat *st, struct fuse_file_info *fi) { (void)path; R(fstat(fi->fh, st)); }
static int pt_access(const char *path, int mask) { P(path); R(access(real, mask)); }
static int pt_readlink(const char *path, char *buf, size_t size) {
  P(path); ssize_t n = readlink(real, buf, size - 1); if (n == -1) return -errno; buf[n] = 0; return 0;
}
static int pt_readdir(const char *path, void *buf, fuse_fill_dir_t fill, off_t off, struct fuse_file_info *fi) {
  (void)off; (void)fi; P(path);
  DIR *d = opendir(real); if (!d) return -errno;
  struct dirent *e;
  while ((e = readdir(d))) {
    struct stat st; memset(&st, 0, sizeof st); st.st_ino = e->d_ino; st.st_mode = DTTOIF(e->d_type);
    if (fill(buf, e->d_name, &st, 0)) break;
  }
  closedir(d); return 0;
}
static int pt_mknod(const char *path, mode_t mode, dev_t dev) { P(path); R(mknod(real, mode, dev)); }
static int pt_mkdir(const char *path, mode_t mode) { P(path); R(mkdir(real, mode)); }
static int pt_unlink(const char *path) { P(path); R(unlink(real)); }
static int pt_rmdir(const char *path) { P(path); R(rmdir(real)); }
static int pt_symlink(const char *to, const char *path) { P(path); R(symlink(to, real)); }
static int pt_rename(const char *from, const char *to) {
  char a[PATH_MAX], b[PATH_MAX]; snprintf(a, sizeof a, "%s%s", root, from); snprintf(b, sizeof b, "%s%s", root, to);
  R(rename(a, b));
}
static int pt_link(const char *from, const char *to) {
  char a[PATH_MAX], b[PATH_MAX]; snprintf(a, sizeof a, "%s%s", root, from); snprintf(b, sizeof b, "%s%s", root, to);
  R(link(a, b));
}
static int pt_chmod(const char *path, mode_t mode) { P(path); R(chmod(real, mode)); }
static int pt_chown(const char *path, uid_t uid, gid_t gid) { P(path); R(lchown(real, uid, gid)); }
static int pt_truncate(const char *path, off_t size) { P(path); R(truncate(real, size)); }
static int pt_ftruncate(const char *path, off_t size, struct fuse_file_info *fi) { (void)path; R(ftruncate(fi->fh, size)); }
static int pt_utimens(const char *path, const struct timespec ts[2]) {
  P(path); struct timeval tv[2];
  tv[0].tv_sec = ts[0].tv_sec; tv[0].tv_usec = ts[0].tv_nsec / 1000;
  tv[1].tv_sec = ts[1].tv_sec; tv[1].tv_usec = ts[1].tv_nsec / 1000;
  R(lutimes(real, tv));
}
static int pt_create(const char *path, mode_t mode, struct fuse_file_info *fi) {
  P(path); int fd = open(real, fi->flags, mode); if (fd == -1) return -errno; fi->fh = fd; return 0;
}
static int pt_open(const char *path, struct fuse_file_info *fi) {
  P(path); int fd = open(real, fi->flags); if (fd == -1) return -errno; fi->fh = fd; return 0;
}
static int pt_read(const char *path, char *buf, size_t size, off_t off, struct fuse_file_info *fi) {
  (void)path; ssize_t n = pread(fi->fh, buf, size, off); return n == -1 ? -errno : (int)n;
}
static int pt_write(const char *path, const char *buf, size_t size, off_t off, struct fuse_file_info *fi) {
  (void)path; ssize_t n = pwrite(fi->fh, buf, size, off); return n == -1 ? -errno : (int)n;
}
static int pt_statfs(const char *path, struct statvfs *st) { P(path); R(statvfs(real, st)); }
static int pt_release(const char *path, struct fuse_file_info *fi) { (void)path; close(fi->fh); return 0; }
static int pt_fsync(const char *path, int datasync, struct fuse_file_info *fi) { (void)path; (void)datasync; R(fsync(fi->fh)); }

static struct fuse_operations ops = {
  .getattr = pt_getattr, .fgetattr = pt_fgetattr, .access = pt_access, .readlink = pt_readlink,
  .readdir = pt_readdir, .mknod = pt_mknod, .mkdir = pt_mkdir, .unlink = pt_unlink, .rmdir = pt_rmdir,
  .symlink = pt_symlink, .rename = pt_rename, .link = pt_link, .chmod = pt_chmod, .chown = pt_chown,
  .truncate = pt_truncate, .ftruncate = pt_ftruncate, .utimens = pt_utimens, .create = pt_create,
  .open = pt_open, .read = pt_read, .write = pt_write, .statfs = pt_statfs, .release = pt_release,
  .fsync = pt_fsync,
};

int main(int argc, char *argv[]) {
  if (argc < 3) { fprintf(stderr, "usage: %s BACKING MOUNTPOINT [options]\n", argv[0]); return 2; }
  if (!realpath(argv[1], root)) { perror(argv[1]); return 1; }
  argv[1] = argv[0];
  return fuse_main(argc - 1, argv + 1, &ops, NULL);
}
