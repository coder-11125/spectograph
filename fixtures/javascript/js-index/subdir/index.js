// Index file that should be resolved when requiring './subdir'
function helper() {
  console.log('Helper function from index.js');
}

module.exports = helper;