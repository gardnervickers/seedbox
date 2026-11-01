#ifndef SBX_GUEST_APPLICATION_H
#define SBX_GUEST_APPLICATION_H
struct sbx_application {
    char **arguments;
    char **execution_prefix;
    char **environment;
    char *working_directory;
    char *user;
    /* Exported to the program as SEEDBOX_GUEST_SOCKET when set; not part of the
     * retained recipe. */
    const char *guest_socket;
};
/* Read the retained counted recipe once before entering the image root. */
int sbx_application_load(struct sbx_application *application, const char *path);
void sbx_application_destroy(struct sbx_application *application);
/* Run only in the forked application child, already inside the image root.
 * Returns errno on setup/exec failure; successful exec never returns. */
int sbx_application_exec(struct sbx_application *application);
#endif
