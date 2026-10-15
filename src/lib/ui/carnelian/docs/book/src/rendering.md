# Rendering

A Carnelian view gets pixels onto the screen by creating
[Rasters](https://fuchsia-docs.firebaseapp.com/rust/carnelian/render/struct.Raster.html#), putting
those Rasters into
[Layers](https://fuchsia-docs.firebaseapp.com/rust/carnelian/render/struct.Layer.html) with a
[Style](https://fuchsia-docs.firebaseapp.com/rust/carnelian/render/generic/struct.Style.html),
adding such layers to a
[Composition](https://fuchsia-docs.firebaseapp.com/rust/carnelian/render/struct.Composition.html),
then passing the composition to the render
[Context](https://fuchsia-docs.firebaseapp.com/rust/carnelian/render/struct.Context.html)'s
[render](https://fuchsia-docs.firebaseapp.com/rust/carnelian/render/struct.Context.html#method.render) method.

Behind the render interface is
[Forma](https://fuchsia-docs.firebaseapp.com/rust/forma/index.html), a CPU renderer.

## Notes

### Rendering Assets

The only way to render pixel content, like what one might load from an PNG file, is via the
[render extensions](https://fuchsia-docs.firebaseapp.com/rust/carnelian/render/struct.RenderExt.html) that are
not fully compatible with the rest of the features of render.

Carnelian does have support for loading vector content from [Rive](https://rive.app) files, which is the best way to
load graphics assets at the time of this writing. The Rive editor available at the Rive home page can import vector
formats and produce Rive files.
