import { address_change } from "./address-change.ts";
import { expense_claim } from "./expense-claim.ts";
import { invoice_dispute } from "./invoice-dispute.ts";
import { member_offboarding } from "./member-offboarding.ts";
import { room_rebooking } from "./room-rebooking.ts";
import type { Workflow } from "./workflow.ts";

export type { Capability, Workflow, WorkflowSite } from "./workflow.ts";

export const workflows: readonly Workflow[] = [
    invoice_dispute,
    member_offboarding,
    expense_claim,
    room_rebooking,
    address_change,
];
