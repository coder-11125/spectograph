// Main file using relative imports
import { helper } from './core/utils.js';
import { otherFunction } from './other/other.js';

function main() {
    console.log(helper());
    console.log(otherFunction());
}

main();