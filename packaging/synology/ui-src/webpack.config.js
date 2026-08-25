const path = require("path");
const { sources } = require("webpack");
const MiniCssExtractPlugin = require("mini-css-extract-plugin");
const VueLoaderPlugin = require("vue-loader/lib/plugin");

class NormalizeCssNewlinePlugin {
  apply(compiler) {
    compiler.hooks.thisCompilation.tap("NormalizeCssNewlinePlugin", (compilation) => {
      compilation.hooks.processAssets.tap(
        {
          name: "NormalizeCssNewlinePlugin",
          stage: compiler.webpack.Compilation.PROCESS_ASSETS_STAGE_SUMMARIZE
        },
        () => {
          const asset = compilation.getAsset("style.css");
          if (asset) {
            const normalized = `${asset.source.source().toString().trimEnd()}\n`;
            compilation.updateAsset("style.css", new sources.RawSource(normalized));
          }
        }
      );
    });
  }
}

module.exports = {
  entry: path.resolve(__dirname, "src/main.js"),
  output: {
    path: path.resolve(__dirname, "dist"),
    filename: "SynologyDriveSync.js",
    clean: true
  },
  externals: {
    vue: "Vue"
  },
  module: {
    rules: [
      {
        test: /\.vue$/,
        loader: "vue-loader"
      },
      {
        test: /\.m?js$/,
        exclude: /node_modules/,
        use: {
          loader: "babel-loader",
          options: {
            presets: [
              ["@babel/preset-env", { modules: false, targets: { chrome: "88" } }]
            ]
          }
        }
      },
      {
        test: /\.css$/,
        use: [MiniCssExtractPlugin.loader, "css-loader"]
      }
    ]
  },
  plugins: [
    new VueLoaderPlugin(),
    new MiniCssExtractPlugin({ filename: "style.css" }),
    new NormalizeCssNewlinePlugin()
  ],
  optimization: {
    splitChunks: false,
    runtimeChunk: false
  },
  performance: {
    hints: "warning",
    maxAssetSize: 512000,
    maxEntrypointSize: 512000
  },
  devtool: false,
  stats: "errors-warnings"
};
