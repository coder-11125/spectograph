// Main file with dynamic imports
import { staticHelper } from './utils.js';

async function loadDynamic() {
    const module = await import('./utils.js');
    return module.dynamicHelper();
}

function main() {
    console.log(staticHelper());
    loadDynamic().then(result => console.log(result));
}

main();