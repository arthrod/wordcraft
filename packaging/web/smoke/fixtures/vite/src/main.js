// The smoke body is shared across bundlers; the package import lives here, inside the
// fixture, so its bare specifier resolves against this fixture's node_modules.
import { start } from "wordcraft-web";
import { run } from "../../main.shared.js";
run(start);
