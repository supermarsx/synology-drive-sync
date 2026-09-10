const path = require("path");
const { sources } = require("webpack");
const MiniCssExtractPlugin = require("mini-css-extract-plugin");
const VueLoaderPlugin = require("vue-loader/lib/plugin");
const condenseCssLoader = require("./build/condense-css-loader");

const runtimeStylesheet = path.resolve(__dirname, "src/styles/native.css");

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

/**
 * Prove the two .css consumers received the same bytes, and fail in the
 * vocabulary of the cause rather than the symptom.
 *
 * The stylesheet is loaded twice below: `resourceQuery: /runtime/` takes it raw
 * through `type: "asset/source"`, and the extracted `style.css` takes it through
 * css-loader and MiniCssExtractPlugin. The bundle has to embed the packaged
 * stylesheet byte for byte, so anything that transforms one path and not the
 * other breaks the package. validate_spk.py catches that, but only afterwards
 * and only as a symptom -- "does not embed the exact packaged stylesheet bytes"
 * -- which reads like a problem with the transform rather than with the fork.
 *
 * condense-css-loader is a pre-loader, so it sits upstream of the fork and both
 * branches start from its output. A pre-loader cannot be bypassed by editing the
 * oneOf branches, but nothing stops a second transform being added to one
 * branch's `use` array later, so that case is detected here rather than assumed
 * away.
 */
class AssertRuntimeStyleParityPlugin {
  apply(compiler) {
    compiler.hooks.thisCompilation.tap("AssertRuntimeStyleParityPlugin", (compilation) => {
      compilation.hooks.processAssets.tap(
        {
          name: "AssertRuntimeStyleParityPlugin",
          // After NormalizeCssNewlinePlugin has settled the trailing byte.
          stage: compiler.webpack.Compilation.PROCESS_ASSETS_STAGE_REPORT
        },
        () => {
          const asset = compilation.getAsset("style.css");
          if (!asset) {
            compilation.errors.push(new Error(
              "AssertRuntimeStyleParityPlugin: no style.css asset was emitted"
            ));
            return;
          }
          const embedded = condenseCssLoader.condensed.get(runtimeStylesheet);
          if (embedded === undefined) {
            compilation.errors.push(new Error([
              `AssertRuntimeStyleParityPlugin: ${runtimeStylesheet} never passed through`,
              "condense-css-loader, so the two .css rules in webpack.config.js are no longer",
              "both fed by the pre-loader upstream of the oneOf fork."
            ].join(" ")));
            return;
          }
          const packaged = asset.source.source().toString();
          if (packaged !== embedded) {
            compilation.errors.push(new Error([
              "AssertRuntimeStyleParityPlugin: the two .css rules in webpack.config.js have diverged.",
              `  The css-loader/MiniCssExtractPlugin path emitted ${packaged.length} chars to dist/style.css,`,
              `  while the resourceQuery:/runtime/ asset/source path embeds ${embedded.length} chars in the bundle.`,
              "  Any transform of the stylesheet must run upstream of that fork (as a pre-loader), or",
              "  identically on both branches. Left alone this fails later in validate_spk.py as",
              '  "native DSM bundle does not embed the exact packaged stylesheet bytes", which names',
              "  the symptom and not this fork."
            ].join("\n")));
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
        // Upstream of the oneOf fork below: a pre-loader runs for every .css
        // module, so both consumers are fed by this one transform. Resolved
        // through require.resolve so webpack loads the same module instance the
        // require above did, and AssertRuntimeStyleParityPlugin sees its output.
        test: /\.css$/,
        enforce: "pre",
        use: require.resolve("./build/condense-css-loader")
      },
      {
        test: /\.css$/,
        oneOf: [
          {
            resourceQuery: /runtime/,
            type: "asset/source"
          },
          {
            use: [
              MiniCssExtractPlugin.loader,
              { loader: "css-loader", options: { url: false } }
            ]
          }
        ]
      }
    ]
  },
  plugins: [
    new VueLoaderPlugin(),
    new MiniCssExtractPlugin({ filename: "style.css" }),
    new NormalizeCssNewlinePlugin(),
    new AssertRuntimeStyleParityPlugin()
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
