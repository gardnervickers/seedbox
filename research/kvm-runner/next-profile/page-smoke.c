#include "clock-page.h"
#include <assert.h>
#include <stdio.h>
int main(void) {
 struct seedbox_clock_page p={.version=1,.flags=1,.sequence=2,
  .machine=4,.incarnation=2,.epoch=1,.monotonic_ns=123};
 uint64_t ns=0;
 assert(seedbox_clock_read(&p,4,2,1,&ns)==0 && ns==123);
 assert(seedbox_clock_read(&p,4,3,1,&ns)==-2);
 p.sequence=3;assert(seedbox_clock_read(&p,4,2,1,&ns)==-1);
 p.sequence=4;p.flags=0;assert(seedbox_clock_read(&p,4,2,1,&ns)==-2);
 puts("{\"status\":\"PASS\",\"scope\":\"C clock-page ABI/read smoke\",\"hardware_or_guest_qualification\":false}");
 return 0;
}
