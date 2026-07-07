# ViewFS
A file system view for the /nix/store.

## Note

While this will work under general purpose use, it was designed for the nix store. Therefore, some assumptions have been made. The major one being that the underlying filesystem is static and will not change while ViewFS is mounted. 
