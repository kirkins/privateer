// The native host compiles this tiny OpenTUI view; control logic is plain JS.
import mail from "./mail.js";

export default {
  id: mail.id,
  setup(ctx) {
    return mail.setup(ctx, status => <text marginRight={1}>{status()}</text>);
  },
};
