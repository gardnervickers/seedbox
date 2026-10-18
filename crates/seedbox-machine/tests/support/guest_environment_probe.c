/* Pure host probe. Every privileged/filesystem setup operation is substituted
 * before including the production header. Assertions and orchestration are Rust.
 * No mount/chroot/reboot/network/sysctl/formatter is performed on the host. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/resource.h>
#include <unistd.h>
#ifdef __linux__
#include <sys/sysmacros.h>
#endif
static void *probe_malloc(size_t);
static void probe_free(void *);
static int probe_open(const char *,int,mode_t);
static int probe_close(int);
static ssize_t probe_read(int,void *,size_t);
static ssize_t probe_write(int,const void *,size_t);
static ssize_t probe_pread(int,void *,size_t,off_t);
static int probe_fchmod(int,mode_t);
static int probe_fstat(int,struct stat *);
static int probe_fchown(int,uid_t,gid_t);
static int probe_fsync(int);
static int probe_mkdir(const char *,mode_t);
static int probe_stat(const char *,struct stat *);
static int probe_lstat(const char *,struct stat *);
static int probe_chmod(const char *,mode_t);
static int probe_chown(const char *,uid_t,gid_t);
static int probe_mount(const char *,const char *,const char *,unsigned long,const void *);
static int probe_mknod(const char *,mode_t,dev_t);
static int probe_symlink(const char *,const char *);
static int probe_hostname(const char *,size_t);
static int probe_rlimit(int,const struct rlimit *);
static int probe_formatter(char *const []);
#define SBX_ENV_MALLOC probe_malloc
#define SBX_ENV_FREE probe_free
#define SBX_ENV_OPEN probe_open
#define SBX_ENV_CLOSE probe_close
#define SBX_ENV_READ probe_read
#define SBX_ENV_WRITE probe_write
#define SBX_ENV_PREAD probe_pread
#define SBX_ENV_FSTAT probe_fstat
#define SBX_ENV_FCHMOD probe_fchmod
#define SBX_ENV_FCHOWN probe_fchown
#define SBX_ENV_FSYNC probe_fsync
#define SBX_ENV_MKDIR probe_mkdir
#define SBX_ENV_STAT probe_stat
#define SBX_ENV_LSTAT probe_lstat
#define SBX_ENV_CHMOD probe_chmod
#define SBX_ENV_CHOWN probe_chown
#define SBX_ENV_MOUNT probe_mount
#define SBX_ENV_MKNOD probe_mknod
#define SBX_ENV_SYMLINK probe_symlink
#define SBX_ENV_SETHOSTNAME probe_hostname
#define SBX_ENV_SETRLIMIT probe_rlimit
#define SBX_ENV_RUN_FORMATTER probe_formatter
#include "guest_environment.h"
static const char *flags="";
static unsigned char *input;
static size_t input_size,input_at,live,allocations,max_allocation,fail_allocation,open_count;
static int loading;static void *record_pointer;static unsigned record_allocated,record_freed;
struct allocation {size_t size;};
static void *probe_malloc(size_t n) {
    allocations++;if(n>max_allocation)max_allocation=n;
    if(fail_allocation==allocations){errno=ENOMEM;return NULL;}
    struct allocation *a=malloc(sizeof(*a)+n);if(!a)return NULL;a->size=n;live++;if(loading && allocations==1){record_pointer=a+1;record_allocated++;}return a+1;
}
static void probe_free(void *p) {if(p){if(p==record_pointer)record_freed++;struct allocation *a=(struct allocation *)p-1;live--;free(a);}}
struct file {char path[SBX_ENV_PATH];int open;size_t written;uint64_t digest;};
static struct file files[128];static size_t file_count;
static const char *file_path(int fd) {return fd>=100 && (size_t)(fd-100)<file_count?files[fd-100].path:"console";}
static int enabled(const char *s){return strstr(flags,s)!=NULL;}
static int probe_open(const char *path,int mode,mode_t permissions) {
    printf("open|%s|%d|%04o\n",path,mode,(unsigned)permissions);
    if(!strcmp(path,"/seedbox/environment.bin") && enabled("absent")){errno=ENOENT;return -1;}
    if(file_count==128){errno=EMFILE;return -1;}
    int fd=(int)file_count+100;snprintf(files[file_count].path,SBX_ENV_PATH,"%s",path);files[file_count].digest=UINT64_C(14695981039346656037);files[file_count++].open=1;open_count++;return fd;
}
static int probe_close(int fd) {printf("close|%s\n",file_path(fd));
    if(enabled("compact") && fd>=100 && files[fd-100].written) {
        printf("content|%s|%zu|%llu\n",file_path(fd),files[fd-100].written,(unsigned long long)files[fd-100].digest);
    }
    if(fd>=100 && files[fd-100].open){files[fd-100].open=0;open_count--;}
    if((enabled("closeconfig")&&!strcmp(file_path(fd),"/etc/demo")) || (enabled("closedev")&&!strcmp(file_path(fd),"/dev/vda")) || (enabled("closeload")&&!strcmp(file_path(fd),"/seedbox/environment.bin"))){errno=EIO;return -1;}
    return 0;}
static int probe_fstat(int fd,struct stat *st) {
    printf("fstat|%s\n",file_path(fd));if(enabled("fstatfail")){errno=EACCES;return -1;}
    memset(st,0,sizeof(*st));st->st_mode=enabled("nonregular")?S_IFIFO:S_IFREG;st->st_size=(off_t)input_size;
    if(enabled("negativesize"))st->st_size=-1;
    if(enabled("oversizedstat"))st->st_size=(off_t)UINT32_MAX+1;
    if(enabled("statsmall"))st->st_size--;
    if(enabled("statlarge"))st->st_size++;
    return 0;
}
static ssize_t probe_read(int fd,void *out,size_t n) {
    size_t available=input_size;if(enabled("shrink")&&available)available--;
    size_t size=input_at<available?available-input_at:0;if(size>n)size=n;
    if(enabled("readerror") || (enabled("bodyfail")&&input_at>=16)){errno=EIO;return -1;}
    if(enabled("grow") && input_at==input_size){*(unsigned char *)out=1;return 1;}
    if(enabled("shortread")&&size>7)size=7;
    memcpy(out,input+input_at,size);input_at+=size;
    if(!enabled("compact")||input_at<=16)printf("read|%s|%zu\n",file_path(fd),size);
    return (ssize_t)size;
}
static ssize_t probe_write(int fd,const void *opaque,size_t n) {
    const char *path=file_path(fd);
    if(enabled("sysctlfail") && !strcmp(path,"/proc/sys/vm/max_map_count")){errno=EACCES;return -1;}
    if((enabled("configfail")&&!strcmp(path,"/etc/demo"))||(enabled("fail-large")&&!strcmp(path,"/x"))){errno=ENOSPC;return -1;}
    if(enabled("shortwrite") && n>2)n=2;
    const unsigned char *bytes=opaque;
    if(enabled("compact")) {
        struct file *file=&files[fd-100];file->written+=n;
        for(size_t i=0;i<n;i++)file->digest=(file->digest^bytes[i])*UINT64_C(1099511628211);
    } else {printf("write|%s|",path);for(size_t i=0;i<n;i++)printf("%02x",bytes[i]);putchar('\n');}
    return (ssize_t)n;
}
static ssize_t probe_pread(int fd,void *out,size_t n,off_t offset) {
    printf("pread|%s|%zu|%lld\n",file_path(fd),n,(long long)offset);
    if(enabled("preadfail")){errno=EIO;return -1;}
    if(n!=2 || offset!=1080){errno=EINVAL;return -1;}
    unsigned char *p=out;p[0]=enabled("existing")?0x53:0;p[1]=enabled("existing")?0xef:0;return 2;
}
static int probe_fchmod(int fd,mode_t mode){printf("fchmod|%s|%04o\n",file_path(fd),(unsigned)mode);return 0;}
static int probe_fchown(int fd,uid_t uid,gid_t gid){printf("fchown|%s|%u|%u\n",file_path(fd),(unsigned)uid,(unsigned)gid);if(enabled("configownerfail")&&!strcmp(file_path(fd),"/etc/demo")){errno=EPERM;return -1;}return 0;}
static int probe_fsync(int fd){printf("fsync|%s\n",file_path(fd));if(enabled("fsyncfail")){errno=EIO;return -1;}return 0;}
static int probe_mkdir(const char *path,mode_t mode) {
    printf("mkdir|%s|%04o\n",path,(unsigned)mode);
    if(!strncmp(path,"/seedbox/disk/",14)) {if(enabled("existing")){errno=EEXIST;return -1;}if(enabled("mkdirfail")){errno=ENOSPC;return -1;}return 0;}
    if(!strcmp(path,"/rootfs") || !strcmp(path,"/rootfs/data") || !strcmp(path,"/rootfs/seedbox") || !strcmp(path,"/dev") || !strcmp(path,"/etc")){errno=EEXIST;return -1;}return 0;
}
static int probe_stat(const char *path,struct stat *st) {
    printf("stat|%s\n",path);if(enabled("missingtarget") && !strcmp(path,"/rootfs/data")){memset(st,0xff,sizeof(*st));errno=ENOENT;return -1;}
    memset(st,0,sizeof(*st));st->st_mode=S_IFDIR|02770;st->st_uid=123;st->st_gid=456;return 0;
}
static int probe_lstat(const char *path,struct stat *st) {
    printf("lstat|%s\n",path);memset(st,0,sizeof(*st));st->st_mode=S_IFDIR|0755;
    if(enabled("symlink") && !strcmp(path,"/rootfs/data"))st->st_mode=S_IFLNK|0777;
    if(enabled("badvolume") && !strncmp(path,"/seedbox/disk/",14))st->st_mode=S_IFREG|0644;
    return 0;
}
static int probe_chmod(const char *p,mode_t mode){printf("chmod|%s|%04o\n",p,(unsigned)mode);return 0;}
static int probe_chown(const char *p,uid_t uid,gid_t gid){printf("chown|%s|%u|%u\n",p,(unsigned)uid,(unsigned)gid);return 0;}
static int probe_mount(const char *source,const char *target,const char *type,unsigned long mode,const void *data) {
    printf("mount|%s|%s|%s|%lu|%s\n",source?source:"-",target,type?type:"-",mode,data?(const char *)data:"-");
    if(enabled("mountfail") && type && !strcmp(type,"ext4")){errno=ENODEV;return -1;}
    if(enabled("rofail") && !strcmp(target,"/etc/demo") && (mode&MS_REMOUNT)){errno=EROFS;return -1;}
    return 0;
}
static int probe_mknod(const char *p,mode_t mode,dev_t dev){printf("mknod|%s|%06o|%u|%u\n",p,(unsigned)mode,(unsigned)major(dev),(unsigned)minor(dev));if(enabled("devicefail")){errno=EPERM;return -1;}return 0;}
static int probe_symlink(const char *target,const char *path){printf("symlink|%s|%s\n",target,path);return 0;}
static int probe_hostname(const char *p,size_t n){printf("hostname|%.*s\n",(int)n,p);if(enabled("hostnamefail")){errno=EPERM;return -1;}return 0;}
static int probe_rlimit(int resource,const struct rlimit *limit){(void)resource;printf("rlimit|%llu|%llu\n",(unsigned long long)limit->rlim_cur,(unsigned long long)limit->rlim_max);if(enabled("rlimitfail")){errno=EPERM;return -1;}return 0;}
static int probe_formatter(char *const args[]) {
    printf("formatter");for(size_t i=0;args[i];i++)printf("|%s",args[i]);putchar('\n');
    if(enabled("formatfail")){errno=EIO;return -1;}return 0;
}
int main(int argc,char **argv) {
    if(argc<2)return 2;
    if(argc>2)flags=argv[2];
    if(!strncmp(flags,"allocfail=",10))fail_allocation=(size_t)strtoul(flags+10,NULL,10);
    /* Probe fixture bound only: largest selected fixture has two16MiB fields
     * plus small metadata. Grow from actual stdin, NEVER a declared length. */
    const size_t fixture_max=2u*SBX_ENV_FIELD+1024u*1024u;
    size_t capacity=8192;input=malloc(capacity);if(!input)return 2;
    for(;;) {
        if(input_size==capacity) {
            if(capacity==fixture_max){free(input);return 2;}
            size_t next=capacity>fixture_max/2?fixture_max:capacity*2;
            unsigned char *grown=realloc(input,next);if(!grown){free(input);return 2;}input=grown;capacity=next;
        }
        ssize_t n=read(STDIN_FILENO,input+input_size,capacity-input_size);
        if(n<0&&errno==EINTR)continue;
        if(n<0){free(input);return 2;}if(!n)break;input_size+=(size_t)n;
    }
    struct sbx_guest_environment env;errno=0;
    loading=!strncmp(argv[1],"load",4);
    int result=loading?sbx_env_load(&env,"/seedbox/environment.bin"):sbx_env_parse(&env,input,input_size);
    int error=errno;loading=0;
    if(result>=0) {
        const unsigned char *record=env.owned_record?env.owned_record:input;
        size_t size=input_size;int valid=1;
        for(uint32_t i=0;i<env.config_count;i++) {
            uintptr_t begin=(uintptr_t)record,at=(uintptr_t)env.configs[i].data;
            if(at<begin || at-begin>size || env.configs[i].size>size-(size_t)(at-begin))valid=0;
        }
        printf("slices|%d|owned|%u\n",valid,env.owned_record!=NULL);
        if(enabled("drop-source")&&!env.owned_record){free(input);return 2;}
        if(enabled("drop-source")){memset(input,0xa6,input_size);free(input);input=NULL;input_size=input_at=0;puts("source-dropped");}
    }
    if(result>=0)printf("parsed|%u|%u|%u|%u|%u|%u|%u\n",env.configured,env.control_socket,env.runtime,env.max_map_count,env.nofile,env.volume_count,env.config_count);
    if(result>=0)printf("control-enabled|%d\n",sbx_env_control_enabled(&env));
    if(result>=0 && strstr(argv[1],"setup")!=NULL) {
        errno=0;result=sbx_env_before_root(&env,enabled("nomanagement")?-1:42);error=errno;
        if(!result){puts("root-switch-stub");result=sbx_env_after_root(&env);error=errno;}
        if(!result){sbx_env_release(&env);printf("released|%zu|%zu\n",live,open_count);puts("start-one");result=sbx_env_application_limits(&env);error=errno;}
        if(!result){puts("start-two");result=sbx_env_application_limits(&env);error=errno;}
    }
    if(result<0)printf("error|%d|%s|%s\n",error,env.failure_phase?env.failure_phase:"parse",env.failure_path);
    else printf("success|%u\n",env.stage);
    sbx_env_destroy(&env);
    printf("record-storage|%u|%u\n",record_allocated,record_freed);
    printf("resources|%zu|%zu|%zu|%zu\n",live,open_count,allocations,max_allocation);
    free(input);return result<0?1:0;
}
