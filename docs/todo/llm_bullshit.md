Now removed, need to find similar and remove:

```
4929 -        // Keep `PermissionResponseWire` exercised so it doesn't rot:                                                                                  
   4930 -        // round-trip the variants so the wire shape stays serialisable.                                                                                      
   4931 -        let _ = PermissionResponseWire::RedirectToChat {                                                                                               
   4932 -            content: "exercise".to_owned(),                                                                                                            
   4933 -        };             
```

crates/frances-workflow/src/runtime.rs is 4000+ lines long
