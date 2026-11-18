#include "macros.h"
typedef struct Node { struct Node* next; int value; } Node;
typedef union Payload { int number; char bytes[4]; } Payload;
enum State { Idle, Busy };
static int local_helper(void) { return 3; }
int apply(int (*callback)(void)) { return callback(); }
int c_run(void) { Node n = {0, 1}; return n.value + local_helper(); }

API_RETURN(int) macro_export(void) { return local_helper(); }

int two(int a, int b) { return a+b; }
int four(int a, int b, int c, int d) { return a+b+c+d; }
#define MANY_CALLS(v) (two(v,v) + four(v,v,v,v))
int macro_many_calls(void) { return MANY_CALLS(1); }
