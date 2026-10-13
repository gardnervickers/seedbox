#define _GNU_SOURCE
#include "guest_application.h"
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <stdlib.h>
int main(int argc,char **argv) {
    if(argc>1&&!strcmp(argv[1],"--child")) { printf("uid=%u arg=%s\n",(unsigned)getuid(),argc>2?argv[2]:"missing");return 0; }
    if(argc>3&&!strcmp(argv[1],"--wrapper")) {execvp(argv[2],argv+2);return errno;}
    if(argc!=2)return 90;
    struct sbx_application app;
    if(sbx_application_load(&app,argv[1]))return 91;
    if (!getuid() && !*app.user) { free(app.user); app.user = strdup("1234:2345"); }
    int error=sbx_application_exec(&app);fprintf(stderr,"exec error=%d\n",error);return error;
}
