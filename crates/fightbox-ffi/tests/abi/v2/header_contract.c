#include "fightbox.h"
#include "abi_declaration_contract.h"
#include "abi_layout_contract.h"

int main(void) {
  /* C11 does not admit a floating macro in an integer constant expression. */
  return FB_MULTIPOINT_FIXED_EXTENT_METERS_V2 == 1.0f ? 0 : 1;
}
