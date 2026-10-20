/* Private configured PID-1 setup. Absent environment.bin preserves the generic
 * helper. No shell, host extraction, cross-machine mounts or anonymous tmpfs.
 * Compile-time syscall substitutions are private host-probe seams only. */
#ifndef SBX_GUEST_ENVIRONMENT_H
#define SBX_GUEST_ENVIRONMENT_H
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/mount.h>
#include <sys/sysmacros.h>
#endif
/* Linux values let non-Linux host probes record exact Linux flags without
 * attempting a host mount. Real guest helpers always compile for Linux. */
#ifndef MS_BIND
#define MS_RDONLY 1
#define MS_NOSUID 2
#define MS_NODEV 4
#define MS_NOEXEC 8
#define MS_REMOUNT 32
#define MS_BIND 4096
#define MS_REC 16384
#endif
#ifndef SBX_ENV_MALLOC
#define SBX_ENV_MALLOC malloc
#define SBX_ENV_FREE free
#endif
#ifndef SBX_ENV_OPEN
#define SBX_ENV_OPEN open
#define SBX_ENV_CLOSE close
#define SBX_ENV_READ read
#define SBX_ENV_WRITE write
#define SBX_ENV_PREAD pread
#define SBX_ENV_FSTAT fstat
#define SBX_ENV_FCHMOD fchmod
#define SBX_ENV_FCHOWN fchown
#define SBX_ENV_FSYNC fsync
#define SBX_ENV_MKDIR mkdir
#define SBX_ENV_STAT stat
#define SBX_ENV_LSTAT lstat
#define SBX_ENV_CHMOD chmod
#define SBX_ENV_CHOWN chown
#define SBX_ENV_MOUNT mount
#define SBX_ENV_MKNOD mknod
#define SBX_ENV_SYMLINK symlink
#define SBX_ENV_SETHOSTNAME sethostname
#define SBX_ENV_SETRLIMIT setrlimit
#endif
#define SBX_ENV_PATH 4096u
#define SBX_ENV_COUNT 65536u
#define SBX_ENV_FILE_MAX UINT32_MAX
#define SBX_ENV_FIELD (16u * 1024u * 1024u)
#define SBX_ENV_ITEMS (7u * SBX_ENV_COUNT)
struct sbx_env_volume { char *name, *target; };
struct sbx_env_config { char *target; const unsigned char *data; uint32_t size; };
struct sbx_guest_environment {
    unsigned configured, control_socket, runtime, stage;
    /* load owns this exact record; parse borrows caller storage instead. */
    unsigned char *owned_record;
    uint32_t max_map_count, nofile;
    char *hostname, *hosts;
    char **anonymous;
    struct sbx_env_volume *volumes;
    struct sbx_env_config *configs;
    uint32_t anonymous_count, volume_count, config_count;
    const char *failure_phase;
    char failure_path[SBX_ENV_PATH];
};
/* Listener selection only: additional host source authorization is separate. */
static inline int sbx_env_control_enabled(const struct sbx_guest_environment *e) {
    return !e->configured || e->control_socket;
}
struct sbx_env_input {
    const unsigned char *bytes;
    size_t size, at;
    uint64_t allocation, allocation_limit;
    uint32_t items;
};
static inline void sbx_env_release(struct sbx_guest_environment *e) {
    int error = errno;
    SBX_ENV_FREE(e->hostname); SBX_ENV_FREE(e->hosts);
    if (e->anonymous) for (uint32_t i=0;i<e->anonymous_count;i++) SBX_ENV_FREE(e->anonymous[i]);
    if (e->volumes) for (uint32_t i=0;i<e->volume_count;i++) { SBX_ENV_FREE(e->volumes[i].name); SBX_ENV_FREE(e->volumes[i].target); }
    if (e->configs) for (uint32_t i=0;i<e->config_count;i++) { SBX_ENV_FREE(e->configs[i].target); }
    SBX_ENV_FREE(e->anonymous); SBX_ENV_FREE(e->volumes); SBX_ENV_FREE(e->configs);
    e->hostname=e->hosts=NULL; e->anonymous=NULL; e->volumes=NULL; e->configs=NULL;
    e->anonymous_count=e->volume_count=e->config_count=0;
    SBX_ENV_FREE(e->owned_record);e->owned_record=NULL;
    errno=error;
}
static inline void sbx_env_destroy(struct sbx_guest_environment *e) {
    int error=errno; sbx_env_release(e); memset(e,0,sizeof(*e)); errno=error;
}
static inline int sbx_env_bad(int error) { errno=error; return -1; }
static inline int sbx_env_error(struct sbx_guest_environment *e,const char *phase,const char *path) {
    int error=errno ? errno : EIO;
    e->failure_phase=phase; snprintf(e->failure_path,sizeof(e->failure_path),"%s",path);
    errno=error; return -1;
}
static inline int sbx_env_u32(struct sbx_env_input *r,uint32_t *out) {
    if (r->size-r->at<4) return sbx_env_bad(EINVAL);
    const unsigned char *b=r->bytes+r->at; r->at+=4;
    *out=(uint32_t)b[0]|(uint32_t)b[1]<<8|(uint32_t)b[2]<<16|(uint32_t)b[3]<<24;
    return 0;
}
static inline int sbx_env_utf8(const unsigned char *s,size_t n) {
    size_t i=0;
    while (i<n) {
        unsigned b=s[i++], remaining, value, minimum;
        if (b<128) continue;
        if (b>=0xc2 && b<=0xdf) {remaining=1;value=b&31;minimum=0x80;}
        else if (b>=0xe0 && b<=0xef) {remaining=2;value=b&15;minimum=0x800;}
        else if (b>=0xf0 && b<=0xf4) {remaining=3;value=b&7;minimum=0x10000;}
        else return -1;
        if (remaining>n-i) return -1;
        while (remaining--) {b=s[i++];if ((b&0xc0)!=0x80) return -1;value=(value<<6)|(b&63);}
        if (value<minimum || value>0x10ffff || (value>=0xd800 && value<=0xdfff)) return -1;
    }
    return 0;
}
static inline void *sbx_env_alloc(struct sbx_env_input *r,size_t n,size_t width,int items) {
    if (width && n>SIZE_MAX/width) {errno=E2BIG;return NULL;}
    size_t bytes=n*width;
    if ((items && n>SBX_ENV_ITEMS-r->items) || bytes>r->allocation_limit-r->allocation) {errno=E2BIG;return NULL;}
    if (items) r->items+=(uint32_t)n;
    r->allocation+=bytes;
    if (!bytes) return NULL;
    void *out=SBX_ENV_MALLOC(bytes);
    if (!out) {errno=ENOMEM;return NULL;}
    memset(out,0,bytes); return out;
}
/* Payload slices stay in the caller/loader record until setup releases it. */
static inline int sbx_env_field(struct sbx_env_input *r,const unsigned char **out,uint32_t *size) {
    uint32_t n;
    if(sbx_env_u32(r,&n))return -1;
    if(n>SBX_ENV_FIELD)return sbx_env_bad(E2BIG);
    if(n>r->size-r->at)return sbx_env_bad(EINVAL);
    *out=r->bytes+r->at;*size=n;r->at+=n;return 0;
}
static inline int sbx_env_text(struct sbx_env_input *r,char **out) {
    const unsigned char *bytes;uint32_t n;
    if(sbx_env_field(r,&bytes,&n))return -1;
    if(memchr(bytes,0,n)||sbx_env_utf8(bytes,n))return sbx_env_bad(EINVAL);
    char *value=sbx_env_alloc(r,(size_t)n+1,1,0);if(!value)return -1;
    memcpy(value,bytes,n);value[n]=0;*out=value;return 0;
}
static inline int sbx_env_path(const char *s,int root) {
    size_t n=strlen(s);
    if (!n || s[0]!='/' || n>=SBX_ENV_PATH || (!root && n==1)) return -1;
    if (n==1) return 0;
    const char *start=s+1;
    for (const char *p=start;;p++) if (!*p || *p=='/') {
        size_t length=(size_t)(p-start);
        if (!length || (length==1 && start[0]=='.') || (length==2 && start[0]=='.' && start[1]=='.')) return -1;
        if (!*p) break;
        start=p+1;
    }
    return 0;
}
static inline int sbx_env_below(const char *s,const char *parent) {
    size_t n=strlen(parent);return !strncmp(s,parent,n) && (!s[n] || s[n]=='/');
}
static inline int sbx_env_config_target(const char *s,int runtime) {
    return sbx_env_path(s,0) || !strcmp(s,"/etc/hosts") || sbx_env_below(s,"/dev") || sbx_env_below(s,"/proc") || sbx_env_below(s,"/sys") || (runtime && sbx_env_below(s,"/seedbox/progress"));
}
static inline int sbx_env_count(struct sbx_env_input *r,uint32_t *out,size_t minimum) {
    if (sbx_env_u32(r,out)) return -1;
    if (*out>SBX_ENV_COUNT) return sbx_env_bad(E2BIG);
    if (*out>(r->size-r->at)/minimum) return sbx_env_bad(EINVAL);
    return 0;
}
/* One explicitly charged index, including names separately from absolute
 * paths. Sort/ancestor lookup never changes authored mount order. */
struct sbx_env_key {const char *path; unsigned kind;};
static inline int sbx_env_key_cmp(const void *left,const void *right) {
    const struct sbx_env_key *a=left,*b=right;
    if ((!a->kind)!=(!b->kind)) return !a->kind ? -1 : 1;
    int order=strcmp(a->path,b->path);if(order)return order;
    return (a->kind>b->kind)-(a->kind<b->kind);
}
static inline unsigned sbx_env_lookup(const struct sbx_env_key *keys,size_t n,const char *path) {
    size_t lo=0,hi=n;
    while(lo<hi) {size_t mid=lo+(hi-lo)/2;if(strcmp(keys[mid].path,path)<0)lo=mid+1;else hi=mid;}
    unsigned kinds=0;while(lo<n && !strcmp(keys[lo].path,path)) {kinds|=1u<<keys[lo].kind;lo++;}return kinds;
}
static inline int sbx_env_collisions(struct sbx_guest_environment *e,struct sbx_env_input *r) {
    size_t n=(size_t)e->anonymous_count+2u*(size_t)e->volume_count+e->config_count;
    struct sbx_env_key *keys=sbx_env_alloc(r,n,sizeof(*keys),1);
    if (n && !keys) return -1;
    size_t at=0;
    for(uint32_t i=0;i<e->anonymous_count;i++)keys[at++]=(struct sbx_env_key){e->anonymous[i],1};
    for(uint32_t i=0;i<e->volume_count;i++) {keys[at++]=(struct sbx_env_key){e->volumes[i].name,0};keys[at++]=(struct sbx_env_key){e->volumes[i].target,2};}
    for(uint32_t i=0;i<e->config_count;i++)keys[at++]=(struct sbx_env_key){e->configs[i].target,3};
    if(n>1)qsort(keys,n,sizeof(*keys),sbx_env_key_cmp);
    int result=0;size_t paths=0;
    while(paths<n && !keys[paths].kind)paths++;
    for(size_t i=0;i<n;) {
        size_t j=i+1;while(j<n && (!keys[j].kind)==(!keys[i].kind) && !strcmp(keys[i].path,keys[j].path))j++;
        if (j-i>1 && (!keys[i].kind || j-i>2 || keys[i].kind!=1 || keys[i+1].kind!=2)) {result=sbx_env_bad(EINVAL);goto done;}
        i=j;
    }
    /* Each ancestor prefix is looked up separately; a linear prefix walk
     * could replace this, but path/count bounds must not become new quotas. */
    for(size_t i=paths;i<n;i++) {
        char copy[SBX_ENV_PATH];size_t length=strlen(keys[i].path);memcpy(copy,keys[i].path,length+1);
        for(size_t p=1;p<length;p++) if(copy[p]=='/') {
            copy[p]=0;unsigned kinds=sbx_env_lookup(keys+paths,n-paths,copy);copy[p]='/';
            if ((kinds&(1u<<3)) || (keys[i].kind==3 && (kinds&(1u<<2)))) {result=sbx_env_bad(EINVAL);goto done;}
        }
    }
done: {int error=errno;SBX_ENV_FREE(keys);errno=error;}return result;
}
/* Borrowed parse: caller keeps bytes alive through writes and release/destroy.
 * Only load transfers owned record storage; destroy never frees caller input. */
static inline int sbx_env_parse(struct sbx_guest_environment *e,const unsigned char *bytes,size_t size) {
    memset(e,0,sizeof(*e));
    if(size>SBX_ENV_FILE_MAX)return sbx_env_bad(E2BIG);
    if(size<16 || memcmp(bytes,"SBXENV1\0",8))return sbx_env_bad(EINVAL);
    uint64_t length=0;for(unsigned i=0;i<8;i++)length|=(uint64_t)bytes[8+i]<<(8*i);
    if(length>(uint64_t)SBX_ENV_FILE_MAX-16)return sbx_env_bad(E2BIG);
    if(length!=(uint64_t)(size-16))return sbx_env_bad(EINVAL);
    /* Input once + copied text bytes no greater than input + one terminator
     * per text (4*COUNT+2), bounded three arrays and <=4*COUNT index keys.
     * All arithmetic is u64 and the record is <=u32::MAX. Logical storage
     * only: excludes allocator overhead/stack/RSS and native admission. */
    uint64_t limit=2*(uint64_t)size+2+(uint64_t)SBX_ENV_COUNT*
        (4+sizeof(char *)+sizeof(struct sbx_env_volume)+sizeof(struct sbx_env_config)+4*sizeof(struct sbx_env_key));
    struct sbx_env_input r={.bytes=bytes+16,.size=(size_t)length,.allocation=size,.allocation_limit=limit};
    if(sbx_env_text(&r,&e->hostname) || sbx_env_text(&r,&e->hosts) || sbx_env_u32(&r,&e->max_map_count) || sbx_env_u32(&r,&e->nofile))goto failed;
    size_t hostname=strlen(e->hostname);
    if(!hostname || hostname>64)goto invalid;
    for(size_t i=0;i<hostname;i++)if((unsigned char)e->hostname[i]<=32 || (unsigned char)e->hostname[i]==127 || e->hostname[i]=='/')goto invalid;
    if(r.size-r.at<2)goto invalid;
    e->control_socket=r.bytes[r.at++];e->runtime=r.bytes[r.at++];
    if(e->control_socket>1 || e->runtime>1 || !e->max_map_count || !e->nofile || (e->runtime && e->nofile<=96))goto invalid;
    if(sbx_env_count(&r,&e->anonymous_count,4))goto failed;
    e->anonymous=sbx_env_alloc(&r,e->anonymous_count,sizeof(*e->anonymous),1);if(e->anonymous_count&&!e->anonymous)goto failed;
    for(uint32_t i=0;i<e->anonymous_count;i++) {
        if(sbx_env_text(&r,&e->anonymous[i]))goto failed;
        if(sbx_env_path(e->anonymous[i],1))goto invalid;
    }
    if(sbx_env_count(&r,&e->volume_count,8))goto failed;
    e->volumes=sbx_env_alloc(&r,e->volume_count,sizeof(*e->volumes),1);if(e->volume_count&&!e->volumes)goto failed;
    for(uint32_t i=0;i<e->volume_count;i++) {
        struct sbx_env_volume *v=&e->volumes[i];
        if(sbx_env_text(&r,&v->name) || sbx_env_text(&r,&v->target))goto failed;
        if(!*v->name || strchr(v->name,'/') || !strcmp(v->name,".") || !strcmp(v->name,"..") || strlen(v->name)>255 || strlen(v->name)+sizeof("/seedbox/disk/")-1>=SBX_ENV_PATH || sbx_env_path(v->target,0) || strlen(v->target)+7>=SBX_ENV_PATH)goto invalid;
    }
    if(sbx_env_count(&r,&e->config_count,8))goto failed;
    e->configs=sbx_env_alloc(&r,e->config_count,sizeof(*e->configs),1);if(e->config_count&&!e->configs)goto failed;
    for(uint32_t i=0;i<e->config_count;i++) {
        struct sbx_env_config *c=&e->configs[i];
        if(sbx_env_text(&r,&c->target) || sbx_env_field(&r,&c->data,&c->size))goto failed;
        if(sbx_env_config_target(c->target,e->runtime) || (i && strcmp(e->configs[i-1].target,c->target)>=0))goto invalid;
    }
    if(r.at!=r.size)goto invalid;
    if(sbx_env_collisions(e,&r))goto failed;
    e->configured=1;return 0;
invalid: errno=EINVAL;
failed: {int error=errno;sbx_env_destroy(e);errno=error;}return -1;
}
static inline int sbx_env_read_exact(int fd,unsigned char *bytes,size_t size) {
    while(size) {ssize_t n=SBX_ENV_READ(fd,bytes,size);if(n<0&&errno==EINTR)continue;if(n<=0){if(!n)errno=EINVAL;return -1;}bytes+=n;size-=(size_t)n;}return 0;
}
/* Returns 0 absent, 1 configured, -1 malformed/unreadable. Allocate only
 * after regular-file actual size and header agree within the newc u32 bound.
 * Exact read+EOF still detect shrink/growth; parse borrows this owned record. */
static inline int sbx_env_load(struct sbx_guest_environment *e,const char *path) {
    memset(e,0,sizeof(*e));int fd=SBX_ENV_OPEN(path,O_RDONLY|O_CLOEXEC,0);
    if(fd<0)return errno==ENOENT?0:-1;
    unsigned char header[16];unsigned char *bytes=NULL;int result=-1;struct stat st;
    if(SBX_ENV_FSTAT(fd,&st))goto done;
    if(!S_ISREG(st.st_mode)||st.st_size<16){errno=EINVAL;goto done;}
    uint64_t actual=(uint64_t)st.st_size;
    if(actual>SBX_ENV_FILE_MAX||actual>SIZE_MAX){errno=E2BIG;goto done;}
    if(sbx_env_read_exact(fd,header,sizeof(header)))goto done;
    if(memcmp(header,"SBXENV1\0",8)){errno=EINVAL;goto done;}
    uint64_t length=0;for(unsigned i=0;i<8;i++)length|=(uint64_t)header[8+i]<<(8*i);
    if(length>(uint64_t)SBX_ENV_FILE_MAX-16||length>SIZE_MAX-16){errno=E2BIG;goto done;}
    if(length!=actual-16){errno=EINVAL;goto done;}
    bytes=SBX_ENV_MALLOC((size_t)actual);if(!bytes){errno=ENOMEM;goto done;}
    memcpy(bytes,header,16);if(sbx_env_read_exact(fd,bytes+16,(size_t)length))goto done;
    unsigned char tail;ssize_t n;do{n=SBX_ENV_READ(fd,&tail,1);}while(n<0&&errno==EINTR);
    if(n){if(n>0)errno=EINVAL;goto done;}
    if(sbx_env_parse(e,bytes,(size_t)actual))goto done;
    e->owned_record=bytes;bytes=NULL;result=1;
done: {int error=errno;SBX_ENV_FREE(bytes);if(SBX_ENV_CLOSE(fd)&&result>=0){result=-1;error=errno;sbx_env_destroy(e);}errno=error;}return result;
}
static inline int sbx_env_directories(const char *path,mode_t mode) {
    char copy[SBX_ENV_PATH];size_t size=strlen(path);
    if(size>=sizeof(copy))return sbx_env_bad(ENAMETOOLONG);
    memcpy(copy,path,size+1);
    for(char *p=copy+1;;p++) {
        if(*p && *p!='/')continue;
        char saved=*p;*p=0;
        if(SBX_ENV_MKDIR(copy,saved?0755:mode)&&errno!=EEXIST)return -1;
        *p=saved;if(!saved)break;
    }
    return 0;
}
static inline int sbx_env_plain_path(const char *path) {
    char copy[SBX_ENV_PATH];size_t size=strlen(path);
    if(size>=sizeof(copy))return sbx_env_bad(ENAMETOOLONG);
    memcpy(copy,path,size+1);
    for(char *p=copy+1;;p++) {
        if(*p && *p!='/')continue;
        char saved=*p;*p=0;struct stat st;
        if(SBX_ENV_LSTAT(copy,&st)){if(errno!=ENOENT)return -1;}
        else if(S_ISLNK(st.st_mode))return sbx_env_bad(ELOOP);
        *p=saved;if(!saved)break;
    }
    return 0;
}
#ifndef SBX_ENV_RUN_FORMATTER
static inline int sbx_env_run_formatter(char *const arguments[]) {
    pid_t pid=fork();if(pid<0)return -1;
    if(!pid){execv("/seedbox/mke2fs",arguments);_exit(127);}
    int status;while(waitpid(pid,&status,0)<0)if(errno!=EINTR)return -1;
    if(!WIFEXITED(status)||WEXITSTATUS(status))return sbx_env_bad(EIO);
    return 0;
}
#define SBX_ENV_RUN_FORMATTER sbx_env_run_formatter
#endif
static inline int sbx_env_format(void) {
    int fd=SBX_ENV_OPEN("/dev/vda",O_RDONLY|O_CLOEXEC,0);if(fd<0)return -1;
    unsigned char magic[2];ssize_t n;do{n=SBX_ENV_PREAD(fd,magic,2,1024+56);}while(n<0&&errno==EINTR);
    int error=errno;if(n!=2){if(n>=0)error=EIO;SBX_ENV_CLOSE(fd);errno=error;return -1;}
    if(SBX_ENV_CLOSE(fd))return -1;
    if(magic[0]==0x53&&magic[1]==0xef)return 0;
    char *const arguments[]={"mke2fs","-q","-F","-t","ext4","-U","5eedb0c5-0000-4000-8000-000000000001","-E","hash_seed=5eedb0c5-0000-4000-8000-000000000002,lazy_itable_init=1,lazy_journal_init=1,nodiscard","/dev/vda",NULL};
    return SBX_ENV_RUN_FORMATTER(arguments);
}
static inline int sbx_env_before_root(struct sbx_guest_environment *e,int management) {
    if(!e->configured)return 0;
    if(e->stage || management<0){errno=EINVAL;return sbx_env_error(e,"retain management","seedbox-lifecycle");}
    if(SBX_ENV_FCHMOD(STDOUT_FILENO,0666))return sbx_env_error(e,"console permissions","/dev/console");
    if(e->volume_count) {
        if(sbx_env_format())return sbx_env_error(e,"format retained disk","/dev/vda");
        if(sbx_env_directories("/seedbox/disk",0700) || SBX_ENV_MOUNT("/dev/vda","/seedbox/disk","ext4",0,"noinit_itable"))return sbx_env_error(e,"mount retained disk","/seedbox/disk");
        int created=0;
        for(uint32_t i=0;i<e->volume_count;i++) {
            char source[SBX_ENV_PATH],target[SBX_ENV_PATH];
            snprintf(source,sizeof(source),"/seedbox/disk/%s",e->volumes[i].name);snprintf(target,sizeof(target),"/rootfs%s",e->volumes[i].target);
            if(sbx_env_plain_path(target))return sbx_env_error(e,"plain volume target",target);
            struct stat image={.st_mode=S_IFDIR|0755,.st_uid=0,.st_gid=0};
            if(SBX_ENV_STAT(target,&image)) {
                if(errno!=ENOENT)return sbx_env_error(e,"stat volume target",target);
                image=(struct stat){.st_mode=S_IFDIR|0755,.st_uid=0,.st_gid=0};
            }
            else if(!S_ISDIR(image.st_mode)){errno=ENOTDIR;return sbx_env_error(e,"volume target",target);}
            if(!SBX_ENV_MKDIR(source,image.st_mode&07777)) {
                if(SBX_ENV_CHOWN(source,image.st_uid,image.st_gid) || SBX_ENV_CHMOD(source,image.st_mode&07777))return sbx_env_error(e,"initialize volume directory",source);
                created=1;
            } else {
                if(errno!=EEXIST)return sbx_env_error(e,"create volume directory",source);
                struct stat existing;
                if(SBX_ENV_LSTAT(source,&existing))return sbx_env_error(e,"existing volume directory",source);
                if(!S_ISDIR(existing.st_mode)){errno=ENOTDIR;return sbx_env_error(e,"existing volume directory",source);}
            }
            if(sbx_env_directories(target,0755) || SBX_ENV_MOUNT(source,target,NULL,MS_BIND,NULL))return sbx_env_error(e,"bind volume",target);
        }
        if(created) {
            int fd=SBX_ENV_OPEN("/seedbox/disk",O_RDONLY|O_DIRECTORY|O_CLOEXEC,0);if(fd<0)return sbx_env_error(e,"open retained root","/seedbox/disk");
            if(SBX_ENV_FSYNC(fd)){int error=errno;SBX_ENV_CLOSE(fd);errno=error;return sbx_env_error(e,"sync volume directories","/seedbox/disk");}
            if(SBX_ENV_CLOSE(fd))return sbx_env_error(e,"close retained root","/seedbox/disk");
        }
    }
    if(e->runtime) {
        const char *target="/rootfs/seedbox/progress";
        if(sbx_env_plain_path(target) || sbx_env_directories(target,0755) || SBX_ENV_MOUNT("/seedbox/progress",target,NULL,MS_BIND,NULL) || SBX_ENV_MOUNT(NULL,target,NULL,MS_BIND|MS_REMOUNT|MS_RDONLY|MS_NOSUID|MS_NODEV,NULL))return sbx_env_error(e,"protect progress runtime",target);
    }
    e->stage=1;return 0;
}
static inline int sbx_env_write_all(int fd,const unsigned char *data,size_t size) {
    while(size){ssize_t n=SBX_ENV_WRITE(fd,data,size);if(n<0&&errno==EINTR)continue;if(n<=0){if(!n)errno=EIO;return -1;}data+=n;size-=(size_t)n;}return 0;
}
static inline int sbx_env_file(const char *path,const void *data,size_t size,mode_t mode) {
    char parent[SBX_ENV_PATH];size_t length=strlen(path);if(length>=sizeof(parent))return sbx_env_bad(ENAMETOOLONG);memcpy(parent,path,length+1);
    char *slash=strrchr(parent,'/');if(slash&&slash!=parent){*slash=0;if(sbx_env_directories(parent,0755))return -1;}
    int fd=SBX_ENV_OPEN(path,O_WRONLY|O_CREAT|O_TRUNC,mode);if(fd<0)return -1;
    int result=sbx_env_write_all(fd,data,size);if(!result)result=SBX_ENV_FCHMOD(fd,mode);if(!result)result=SBX_ENV_FCHOWN(fd,0,0);
    int error=errno;if(SBX_ENV_CLOSE(fd)&&!result){result=-1;error=errno;}errno=error;return result;
}
static inline int sbx_env_sysctl(const char *path,uint32_t value) {
    char bytes[32];int length=snprintf(bytes,sizeof(bytes),"%u\n",value);
    int fd=SBX_ENV_OPEN(path,O_WRONLY|O_CLOEXEC,0);if(fd<0)return -1;
    ssize_t n;do{n=SBX_ENV_WRITE(fd,bytes,(size_t)length);}while(n<0&&errno==EINTR);
    int error=errno;if(n!=length){if(n>=0)error=EIO;SBX_ENV_CLOSE(fd);errno=error;return -1;}return SBX_ENV_CLOSE(fd);
}
static inline int sbx_env_devices(struct sbx_guest_environment *e) {
    if(sbx_env_directories("/dev",0755) || SBX_ENV_MOUNT("tmpfs","/dev","tmpfs",MS_NOSUID|MS_NOEXEC,"mode=0755,size=65536"))return sbx_env_error(e,"application dev","/dev");
    const struct {const char *name;unsigned minor;} nodes[]={{"null",3},{"zero",5},{"full",7},{"random",8},{"urandom",9},{"tty",0}};
    for(size_t i=0;i<sizeof(nodes)/sizeof(*nodes);i++) {
        char path[64];snprintf(path,sizeof(path),"/dev/%s",nodes[i].name);
        if(SBX_ENV_MKNOD(path,S_IFCHR|0666,makedev(i==5?5:1,nodes[i].minor)) || SBX_ENV_CHMOD(path,0666))return sbx_env_error(e,"application device",path);
    }
    if(sbx_env_directories("/dev/shm",01777) || SBX_ENV_MOUNT("tmpfs","/dev/shm","tmpfs",MS_NOSUID|MS_NODEV|MS_NOEXEC,"mode=1777,size=67108864"))return sbx_env_error(e,"application shm","/dev/shm");
    if(sbx_env_directories("/dev/pts",0755) || SBX_ENV_MOUNT("devpts","/dev/pts","devpts",MS_NOSUID|MS_NOEXEC,"newinstance,ptmxmode=0666,mode=0620,gid=5"))return sbx_env_error(e,"application pts","/dev/pts");
    const char *links[][2]={{"pts/ptmx","/dev/ptmx"},{"/proc/self/fd","/dev/fd"},{"/proc/self/fd/0","/dev/stdin"},{"/proc/self/fd/1","/dev/stdout"},{"/proc/self/fd/2","/dev/stderr"}};
    for(size_t i=0;i<sizeof(links)/sizeof(*links);i++)if(SBX_ENV_SYMLINK(links[i][0],links[i][1]))return sbx_env_error(e,"application device link",links[i][1]);
    return 0;
}
/* Caller already entered the application mount root and mounted proc/sys.
 * This never repeats on process Stop/Start. No formatter is executed here. */
static inline int sbx_env_after_root(struct sbx_guest_environment *e) {
    if(!e->configured)return 0;
    if(e->stage!=1){errno=EINVAL;return sbx_env_error(e,"setup order","application root");}
    if(sbx_env_devices(e))return -1;
    if(SBX_ENV_SETHOSTNAME(e->hostname,strlen(e->hostname)))return sbx_env_error(e,"hostname",e->hostname);
    char hostname[66];int size=snprintf(hostname,sizeof(hostname),"%s\n",e->hostname);
    if(sbx_env_file("/etc/hostname",hostname,(size_t)size,0644) || sbx_env_file("/etc/hosts",e->hosts,strlen(e->hosts),0644))return sbx_env_error(e,"hostname files","/etc/hostname or /etc/hosts");
    if(sbx_env_sysctl("/proc/sys/vm/max_map_count",e->max_map_count))return sbx_env_error(e,"application sysctl","/proc/sys/vm/max_map_count");
    if(sbx_env_sysctl("/proc/sys/fs/nr_open",e->nofile))return sbx_env_error(e,"application sysctl","/proc/sys/fs/nr_open");
    for(uint32_t i=0;i<e->anonymous_count;i++)if(sbx_env_directories(e->anonymous[i],0755))return sbx_env_error(e,"anonymous volume directory",e->anonymous[i]);
    for(uint32_t i=0;i<e->config_count;i++) {
        struct sbx_env_config *c=&e->configs[i];
        if(sbx_env_file(c->target,c->data,c->size,0444) || SBX_ENV_MOUNT(c->target,c->target,NULL,MS_BIND,NULL) || SBX_ENV_MOUNT(NULL,c->target,NULL,MS_BIND|MS_REMOUNT|MS_RDONLY,NULL))return sbx_env_error(e,"read-only config",c->target);
    }
    e->stage=2;return 0;
}
/* Child-only, before credentials, on every application Start. */
static inline int sbx_env_application_limits(struct sbx_guest_environment *e) {
    if(!e->configured)return 0;
    if(e->stage!=2){errno=EINVAL;return sbx_env_error(e,"application limit order","RLIMIT_NOFILE");}
    struct rlimit limit={.rlim_cur=e->nofile,.rlim_max=e->nofile};
    if(SBX_ENV_SETRLIMIT(RLIMIT_NOFILE,&limit))return sbx_env_error(e,"application nofile limit","RLIMIT_NOFILE");
    return 0;
}
#endif
